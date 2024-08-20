//! Implementation of BlockBuildingAlgorithm that sorts the SimulatedOrders by some criteria.
//! After sorting it starts from an empty block and tries to add the SimulatedOrders one by one keeping on the block only the successful ones.
//! If a SimulatedOrder gives less profit than the value it gave on the top of block simulation is considered as failed (ExecutionError::LowerInsertedValue)
//! but it can be later reused.
//! The described algorithm is ran continuously adding new SimulatedOrders (they arrive on real time!) on each iteration until we run out of time (slot ends).
//! Sorting criteria are described on [`Sorting`].
//! For some more details see [`OrderingBuilderConfig`]
use crate::{
    building::{
        block_orders_from_sim_orders,
        builders::{
            block_building_helper::BlockBuildingHelper, LiveBuilderInput, OrderIntakeConsumer,
        },
        BlockBuildingContext, BlockOrders, ExecutionError, Sorting,
    },
    primitives::{AccountNonce, OrderId},
    utils::is_provider_factory_health_error,
};
use ahash::{HashMap, HashSet};
use alloy_primitives::Address;
use reth::providers::{BlockNumReader, ProviderFactory};
use reth_db::database::Database;

use crate::{
    live_builder::bidding::SlotBidder, roothash::RootHashMode, utils::check_provider_factory_health,
};
use reth::tasks::pool::BlockingTaskPool;
use reth_payload_builder::database::CachedReads;
use serde::Deserialize;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use time::OffsetDateTime;
use tracing::{debug, error, info_span, trace, warn};

use super::{
    BacktestSimulateBlockInput, Block, BlockBuildingAlgorithm, BlockBuildingAlgorithmInput,
    BlockBuildingSink,
};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OrderingBuilderConfig {
    /// If a tx inside a bundle or sbundle fails with TransactionErr (don't confuse this with reverting which is TransactionOk with !.receipt.success)
    /// and it's configured as allowed to revert (for bundles tx in reverting_tx_hashes, for sbundles: TxRevertBehavior != NotAllowed) we continue the
    /// the execution of the bundle/sbundle
    pub discard_txs: bool,
    pub sorting: Sorting,
    /// Only when a tx fails because the profit was worst than expected: Number of time an order can fail during a single block building iteration.
    /// When thi happens it gets reinserted in the BlockStore with the new simulated profit (the one that failed).
    pub failed_order_retries: usize,
    /// if a tx fails in a block building iteration it's dropped so next iterations will not use it.
    pub drop_failed_orders: bool,
    /// Start the first iteration of block building using direct pay to fee_recipient (validator)
    /// This mode saves gas on the payout tx from builder to validator but disables mev-share and profit taking.
    #[serde(default)]
    pub coinbase_payment: bool,
    /// Amount of time allocated for EVM execution while building block.
    #[serde(default)]
    pub build_duration_deadline_ms: Option<u64>,
}

impl OrderingBuilderConfig {
    pub fn build_duration_deadline(&self) -> Option<Duration> {
        self.build_duration_deadline_ms.map(Duration::from_millis)
    }
}

pub fn run_ordering_builder<
    Provider: StateProviderFactory + Clone + 'static,
    SinkType: BlockBuildingSink,
>(
    input: LiveBuilderInput<Provider, SinkType>,
    config: &OrderingBuilderConfig,
) {
    let block_number = input.ctx.block_env.number.to::<u64>();
    //
    let mut order_intake_consumer = OrderIntakeConsumer::new(
        input.provider_factory.clone(),
        input.input,
        input.ctx.attributes.parent,
        config.sorting,
        &input.sbundle_mergeabe_signers,
    );

    let mut builder = OrderingBuilderContext::new(
        input.provider_factory.clone(),
        input.slot_bidder,
        input.root_hash_task_pool,
        input.builder_name,
        input.ctx,
        config.clone(),
    );

    // this is a hack to mark used orders until built block trace is implemented as a sane thing
    let mut removed_orders = Vec::new();
    let mut use_suggested_fee_recipient_as_coinbase = config.coinbase_payment;
    'building: loop {
        if input.cancel.is_cancelled() {
            break 'building;
        }

        match order_intake_consumer.consume_next_batch() {
            Ok(ok) => {
                if !ok {
                    break 'building;
                }
            }
            Err(err) => {
                error!(?err, "Error consuming next order batch");
                continue;
            }
        }

        let orders = order_intake_consumer.current_block_orders();
        match builder.build_block(orders, use_suggested_fee_recipient_as_coinbase) {
            Ok(Some(block)) => {
                if block.trace.got_no_signer_error {
                    use_suggested_fee_recipient_as_coinbase = false;
                }
                input.sink.new_block(block);
            }
            Ok(None) => {}
            Err(err) => {
                // @Types
                let err_str = err.to_string();
                if err_str.contains("failed to initialize consistent view") {
                    let last_block_number = input
                        .provider_factory
                        .last_block_number()
                        .unwrap_or_default();
                    debug!(
                        block_number,
                        last_block_number, "Can't build on this head, cancelling slot"
                    );
                    input.cancel.cancel();
                    break 'building;
                } else if !err_str.contains("Profit too low") {
                    if is_provider_factory_health_error(&err) {
                        error!(?err, "Cancelling building due to provider factory error");
                        break 'building;
                    } else {
                        warn!(?err, "Error filling orders");
                    }
                }
            }
        }
        if config.drop_failed_orders {
            let mut removed = order_intake_consumer.remove_orders(builder.failed_orders.drain());
            removed_orders.append(&mut removed);
        }
    }
}

pub fn backtest_simulate_block<Provider: StateProviderFactory + Clone + 'static>(
    ordering_config: OrderingBuilderConfig,
    input: BacktestSimulateBlockInput<'_, Provider>,
) -> eyre::Result<(Block, CachedReads)> {
    let use_suggested_fee_recipient_as_coinbase = ordering_config.coinbase_payment;
    let state_provider = input
        .provider_factory
        .history_by_block_number(input.ctx.block_env.number.to::<u64>() - 1)?;
    let block_orders = block_orders_from_sim_orders(
        input.sim_orders,
        ordering_config.sorting,
        &state_provider,
        &input.sbundle_mergeabe_signers,
    )?;
    let mut builder = OrderingBuilderContext::new(
        input.provider_factory.clone(),
        Arc::new(()),
        BlockingTaskPool::build()?,
        input.builder_name,
        input.ctx.clone(),
        ordering_config,
    )
    .with_skip_root_hash()
    .with_cached_reads(input.cached_reads.unwrap_or_default());
    let block = builder
        .build_block(block_orders, use_suggested_fee_recipient_as_coinbase)?
        .ok_or_else(|| eyre::eyre!("No block built"))?;
    Ok((block, builder.take_cached_reads().unwrap_or_default()))
}

#[derive(Debug)]
pub struct OrderingBuilderContext<Provider> {
    provider_factory: Provider,
    root_hash_task_pool: BlockingTaskPool,
    builder_name: String,
    ctx: BlockBuildingContext,
    config: OrderingBuilderConfig,
    root_hash_mode: RootHashMode,
    slot_bidder: Arc<dyn SlotBidder>,

    // caches
    cached_reads: Option<CachedReads>,

    // scratchpad
    failed_orders: HashSet<OrderId>,
    order_attempts: HashMap<OrderId, usize>,
}

impl<Provider: StateProviderFactory + Clone + 'static> OrderingBuilderContext<Provider> {
    pub fn new(
        provider_factory: Provider,
        slot_bidder: Arc<dyn SlotBidder>,
        root_hash_task_pool: BlockingTaskPool,
        builder_name: String,
        ctx: BlockBuildingContext,
        config: OrderingBuilderConfig,
    ) -> Self {
        Self {
            provider_factory,
            root_hash_task_pool,
            builder_name,
            ctx,
            config,
            root_hash_mode: RootHashMode::CorrectRoot,
            slot_bidder,
            cached_reads: None,
            failed_orders: HashSet::default(),
            order_attempts: HashMap::default(),
        }
    }

    /// Should be used only in backtest
    pub fn with_skip_root_hash(self) -> Self {
        Self {
            root_hash_mode: RootHashMode::SkipRootHash,
            ..self
        }
    }

    pub fn with_cached_reads(self, cached_reads: CachedReads) -> Self {
        Self {
            cached_reads: Some(cached_reads),
            ..self
        }
    }

    pub fn take_cached_reads(&mut self) -> Option<CachedReads> {
        self.cached_reads.take()
    }

    /// use_suggested_fee_recipient_as_coinbase: all the mev profit goes directly to the slot suggested_fee_recipient so we avoid the payout tx.
    ///     This mode disables mev-share orders since the builder has to receive the mev profit to give some portion back to the mev-share user.
    /// !use_suggested_fee_recipient_as_coinbase: all the mev profit goes to the builder and at the end of the block we pay to the suggested_fee_recipient.
    pub fn build_block(
        &mut self,
        block_orders: BlockOrders,
        use_suggested_fee_recipient_as_coinbase: bool,
    ) -> eyre::Result<Option<Block>> {
        let use_suggested_fee_recipient_as_coinbase = use_suggested_fee_recipient_as_coinbase
            && self.slot_bidder.is_pay_to_coinbase_allowed();

        let build_attempt_id: u32 = rand::random();
        let span = info_span!("build_run", build_attempt_id);
        let _guard = span.enter();

        check_provider_factory_health(self.ctx.block(), &self.provider_factory)?;

        let build_start = Instant::now();
        let orders_closed_at = OffsetDateTime::now_utc();

        // Create a new ctx to remove builder_signer if necessary
        let mut new_ctx = self.ctx.clone();
        if use_suggested_fee_recipient_as_coinbase {
            new_ctx.modify_use_suggested_fee_recipient_as_coinbase();
        }
        self.failed_orders.clear();
        self.order_attempts.clear();

        let mut block_building_helper = BlockBuildingHelper::new(
            self.provider_factory.clone(),
            new_ctx,
            self.cached_reads.take(),
            self.builder_name.clone(),
            self.config.discard_txs,
            self.config.sorting.into(),
        )?;

        self.fill_orders(&mut block_building_helper, block_orders, build_start)?;
        block_building_helper.set_trace_fill_time(build_start.elapsed());

        let finalize_block_result = block_building_helper.finalize_block(
            self.slot_bidder.as_ref(),
            orders_closed_at,
            self.root_hash_task_pool.clone(),
            self.root_hash_mode,
        )?;
        self.cached_reads = Some(finalize_block_result.cached_reads);
        Ok(finalize_block_result.block)
    }

    fn fill_orders(
        &mut self,
        block_building_helper: &mut BlockBuildingHelper<DB>,
        mut block_orders: BlockOrders,
        build_start: Instant,
    ) -> eyre::Result<()> {
        let mut order_attempts: HashMap<OrderId, usize> = HashMap::default();
        // @Perf when gas left is too low we should break.
        while let Some(sim_order) = block_orders.pop_order() {
            if let Some(deadline) = self.config.build_duration_deadline() {
                if build_start.elapsed() > deadline {
                    break;
                }
            }
            let start_time = Instant::now();
            let commit_result = block_building_helper.commit_order(&sim_order)?;
            let order_commit_time = start_time.elapsed();
            let mut gas_used = 0;
            let mut execution_error = None;
            let mut reinserted = false;
            let success = commit_result.is_ok();
            match commit_result {
                Ok(res) => {
                    gas_used = res.gas_used;
                    // This intermediate step is needed until we replace all (Address, u64) for AccountNonce
                    let nonces_updated: Vec<_> = res
                        .nonces_updated
                        .iter()
                        .map(|(account, nonce)| AccountNonce {
                            account: *account,
                            nonce: *nonce,
                        })
                        .collect();
                    block_orders.update_onchain_nonces(&nonces_updated);
                }
                Err(err) => {
                    if let ExecutionError::LowerInsertedValue { inplace, .. } = &err {
                        // try to reinsert order into the map
                        let order_attempts = order_attempts.entry(sim_order.id()).or_insert(0);
                        if *order_attempts < self.config.failed_order_retries {
                            let mut new_order = sim_order.clone();
                            new_order.sim_value = inplace.clone();
                            block_orders.readd_order(new_order);
                            *order_attempts += 1;
                            reinserted = true;
                        }
                    }
                    if !reinserted {
                        self.failed_orders.insert(sim_order.id());
                    }
                    execution_error = Some(err);
                }
            }
            trace!(
                order_id = ?sim_order.id(),
                success,
                order_commit_time_mus = order_commit_time.as_micros(),
                gas_used,
                ?execution_error,
                reinserted,
                "Executed order"
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct OrderingBuildingAlgorithm {
    root_hash_task_pool: BlockingTaskPool,
    sbundle_mergeabe_signers: Vec<Address>,
    config: OrderingBuilderConfig,
    name: String,
}

impl OrderingBuildingAlgorithm {
    pub fn new(
        root_hash_task_pool: BlockingTaskPool,
        sbundle_mergeabe_signers: Vec<Address>,
        config: OrderingBuilderConfig,
        name: String,
    ) -> Self {
        Self {
            root_hash_task_pool,
            sbundle_mergeabe_signers,
            config,
            name,
        }
    }
}

impl<Provider: StateProviderFactory + Clone + 'static, SinkType: BlockBuildingSink>
    BlockBuildingAlgorithm<Provider, SinkType> for OrderingBuildingAlgorithm
{
    fn name(&self) -> String {
        self.name.clone()
    }

    fn build_blocks(&self, input: BlockBuildingAlgorithmInput<Provider, SinkType>) {
        let live_input = LiveBuilderInput {
            provider_factory: input.provider_factory,
            root_hash_task_pool: self.root_hash_task_pool.clone(),
            ctx: input.ctx.clone(),
            input: input.input,
            sink: input.sink,
            builder_name: self.name.clone(),
            slot_bidder: input.slot_bidder,
            cancel: input.cancel,
            sbundle_mergeabe_signers: self.sbundle_mergeabe_signers.clone(),
        };
        run_ordering_builder(live_input, &self.config);
    }
}
