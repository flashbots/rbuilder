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
            block_building_helper::BlockBuildingHelper, BuiltBlockId, LiveBuilderInput,
            OrderIntakeConsumer,
        },
        order_is_worth_executing, BlockBuildingContext, ExecutionError,
        NullPartialBlockExecutionTracer, OrderPriority, PartialBlockExecutionTracer,
        PrioritizedOrderStore, SimulatedOrderSink, Sorting, ThreadBlockBuildingContext,
    },
    live_builder::building::built_block_cache::BuiltBlockCache,
    provider::StateProviderFactory,
    telemetry::{
        add_ordering_builder_base_stage_stats, add_ordering_builder_pre_filtered_stage_stats,
        mark_builder_considers_order, OrderInclusionRatio,
    },
    utils::NonceCache,
};
use ahash::{HashMap, HashSet};
use alloy_primitives::I256;
use derivative::Derivative;
use rbuilder_primitives::{AccountNonce, OrderId, SimValue, SimulatedOrder};
use reth_provider::StateProvider;
use serde::Deserialize;
use std::{
    marker::PhantomData,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info_span, trace};

use super::{
    block_building_helper::{BiddableUnfinishedBlock, BlockBuildingHelperFromProvider},
    handle_building_error, BacktestSimulateBlockInput, Block, BlockBuildingAlgorithm,
    BlockBuildingAlgorithmInput,
};

pub fn default_pre_filtered_build_duration_deadline_ms() -> Option<u64> {
    Some(0)
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OrderingBuilderConfig {
    /// If a tx inside a bundle or sbundle fails with TransactionErr (don't confuse this with reverting which is TransactionOk with !.receipt.success)
    /// and it's configured as allowed to revert (for bundles tx in reverting_tx_hashes or dropping_tx_hashes, for sbundles: TxRevertBehavior != NotAllowed)
    /// we continue the  execution of the bundle/sbundle. The most typical value is true.
    pub discard_txs: bool,
    pub sorting: Sorting,
    /// Only when a tx fails because the profit was worst than expected: Number of time an order can fail during a single block building iteration.
    /// When thi happens it gets reinserted in the PrioritizedOrderStore with the new simulated profit (the one that failed).
    pub failed_order_retries: usize,
    /// if a tx fails in a block building iteration it's dropped so next iterations will not use it.
    pub drop_failed_orders: bool,
    /// Amount of time allocated for EVM execution while building block.
    #[serde(default)]
    pub build_duration_deadline_ms: Option<u64>,
    /// Amount of time allocated for EVM execution for the second stage in which we only try orders that worked for other builders.
    #[serde(default = "default_pre_filtered_build_duration_deadline_ms")]
    pub pre_filtered_build_duration_deadline_ms: Option<u64>,
    #[serde(default)]
    /// Use SimValue::non_mempool_profit_info instead of full_profit_info when comparing Orders.
    pub ignore_mempool_profit_on_bundles: bool,
}

impl OrderingBuilderConfig {
    pub fn build_duration_deadline(&self) -> Option<Duration> {
        self.build_duration_deadline_ms.map(Duration::from_millis)
    }
    pub fn pre_filtered_build_duration_deadline(&self) -> Option<Duration> {
        self.pre_filtered_build_duration_deadline_ms
            .map(Duration::from_millis)
    }
}

pub fn run_ordering_builder<P, OrderPriorityType>(
    input: LiveBuilderInput<P>,
    config: &OrderingBuilderConfig,
) where
    P: StateProviderFactory + Clone + 'static,
    OrderPriorityType: OrderPriority,
{
    let payload_id = input.ctx.payload_id;

    let block_state: Arc<dyn StateProvider> = match input
        .provider
        .history_by_block_hash(input.ctx.attributes.parent)
    {
        Ok(state) => Arc::from(state),
        Err(err) => {
            error!(
                ?err,
                payload_id,
                builder = input.builder_name,
                "Failed to get history_by_block_hash, cancelling builder job"
            );
            return;
        }
    };

    let nonces = NonceCache::new(block_state.clone());

    let mut order_intake_consumer =
        OrderIntakeConsumer::<OrderPriorityType>::new(nonces, input.input);

    let mut builder = OrderingBuilderContext::new(
        block_state.clone(),
        input.builder_name,
        input.ctx,
        config.clone(),
        input.max_order_execution_duration_warning,
        input.built_block_cache,
    );

    // this is a hack to mark used orders until built block trace is implemented as a sane thing
    let mut removed_orders = Vec::new();
    'building: loop {
        if input.cancel.is_cancelled() {
            break 'building;
        }

        match order_intake_consumer.blocking_consume_next_batch() {
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
        match builder.build_block(
            orders,
            input.built_block_id_source.get_new_id(),
            input.cancel.clone(),
        ) {
            Ok(block) => {
                if let Ok(block) = BiddableUnfinishedBlock::new(block) {
                    input.sink.new_block(block);
                }
            }
            Err(err) => {
                if !handle_building_error(err, payload_id) {
                    break 'building;
                }
            }
        }
        if config.drop_failed_orders {
            let mut removed = order_intake_consumer.remove_orders(builder.failed_orders.drain());
            removed_orders.append(&mut removed);
        }
    }
}

pub fn backtest_simulate_block<
    P,
    OrderPriorityType: OrderPriority,
    PartialBlockExecutionTracerType: PartialBlockExecutionTracer + Clone + Send + Sync + 'static,
>(
    ordering_config: OrderingBuilderConfig,
    input: BacktestSimulateBlockInput<'_, P>,
    partial_block_execution_tracer: PartialBlockExecutionTracerType,
) -> eyre::Result<Block>
where
    P: StateProviderFactory + Clone + 'static,
{
    let state_provider = input
        .provider
        .history_by_block_number(input.ctx.block() - 1)?;
    let block_orders =
        block_orders_from_sim_orders::<OrderPriorityType>(input.sim_orders, &state_provider)?;
    let mut local_ctx = ThreadBlockBuildingContext::default();
    let mut builder = OrderingBuilderContext::new(
        Arc::from(state_provider),
        input.builder_name,
        input.ctx.clone(),
        ordering_config,
        None,
        Arc::new(BuiltBlockCache::new()),
    );
    let mut block_builder = builder.build_block_with_execution_tracer(
        block_orders,
        BuiltBlockId::ZERO,
        CancellationToken::new(),
        partial_block_execution_tracer,
    )?;

    let payout_tx_value = block_builder.true_block_value()?;
    let finalize_block_result =
        block_builder.finalize_block(&mut local_ctx, payout_tx_value, I256::ZERO, None)?;
    Ok(finalize_block_result.block)
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct OrderingBuilderContext {
    #[derivative(Debug = "ignore")]
    state: Arc<dyn StateProvider>,
    builder_name: String,
    ctx: BlockBuildingContext,
    config: OrderingBuilderConfig,
    /// See [BlockBuildingHelperFromProvider::max_order_execution_duration_warning]
    max_order_execution_duration_warning: Option<Duration>,

    // caches
    local_ctx: ThreadBlockBuildingContext,

    // scratchpad
    failed_orders: HashSet<OrderId>,
    order_attempts: HashMap<OrderId, usize>,
    built_block_cache: Arc<BuiltBlockCache>,
}

impl OrderingBuilderContext {
    pub fn new(
        state: Arc<dyn StateProvider>,
        builder_name: String,
        ctx: BlockBuildingContext,
        config: OrderingBuilderConfig,
        max_order_execution_duration_warning: Option<Duration>,
        built_block_cache: Arc<BuiltBlockCache>,
    ) -> Self {
        Self {
            state,
            builder_name,
            ctx,
            local_ctx: Default::default(),
            config,
            failed_orders: HashSet::default(),
            order_attempts: HashMap::default(),
            built_block_cache,
            max_order_execution_duration_warning,
        }
    }

    pub fn build_block<OrderPriorityType: OrderPriority>(
        &mut self,
        block_orders: PrioritizedOrderStore<OrderPriorityType>,
        built_block_id: BuiltBlockId,
        cancel_block: CancellationToken,
    ) -> eyre::Result<Box<dyn BlockBuildingHelper>> {
        self.build_block_with_execution_tracer(
            block_orders,
            built_block_id,
            cancel_block,
            NullPartialBlockExecutionTracer {},
        )
    }

    /// use_suggested_fee_recipient_as_coinbase: all the mev profit goes directly to the slot suggested_fee_recipient so we avoid the payout tx.
    ///     This mode disables mev-share orders since the builder has to receive the mev profit to give some portion back to the mev-share user.
    /// !use_suggested_fee_recipient_as_coinbase: all the mev profit goes to the builder and at the end of the block we pay to the suggested_fee_recipient.
    pub fn build_block_with_execution_tracer<
        OrderPriorityType: OrderPriority,
        PartialBlockExecutionTracerType: PartialBlockExecutionTracer + Clone + Send + Sync + 'static,
    >(
        &mut self,
        mut block_orders: PrioritizedOrderStore<OrderPriorityType>,
        built_block_id: BuiltBlockId,
        cancel_block: CancellationToken,
        partial_block_execution_tracer: PartialBlockExecutionTracerType,
    ) -> eyre::Result<Box<dyn BlockBuildingHelper>> {
        let build_attempt_id: u32 = rand::random();
        let span = info_span!("build_run", build_attempt_id);
        let _guard = span.enter();

        let build_start = Instant::now();

        // Create a new ctx to remove builder_signer if necessary
        self.failed_orders.clear();
        self.order_attempts.clear();

        let mut block_building_helper = BlockBuildingHelperFromProvider::new_with_execution_tracer(
            built_block_id,
            self.state.clone(),
            self.ctx.clone(),
            &mut self.local_ctx,
            self.builder_name.clone(),
            self.config.discard_txs,
            block_orders.orders_statistics(),
            cancel_block,
            partial_block_execution_tracer,
            self.max_order_execution_duration_warning,
        )?;
        self.fill_orders(
            &mut block_building_helper,
            &mut block_orders,
            |_| true,
            build_start,
            self.config.build_duration_deadline(),
        )?;
        add_ordering_builder_base_stage_stats(
            self.builder_name.as_str(),
            OrderInclusionRatio::new_from_failed(
                block_building_helper
                    .built_block_trace()
                    .considered_orders_statistics
                    .total(),
                block_building_helper
                    .built_block_trace()
                    .failed_orders_statistics
                    .total(),
            ),
        );
        if self.config.pre_filtered_build_duration_deadline_ms != Some(0) {
            // Consider aggregate all the BuiltBlockInfos.
            let block_infos = self.built_block_cache.get_block_infos(&self.builder_name);
            if !block_infos.is_empty() {
                let base_considered_orders_statistics = block_building_helper
                    .built_block_trace()
                    .considered_orders_statistics
                    .clone();
                let base_failed_orders_statistics = block_building_helper
                    .built_block_trace()
                    .failed_orders_statistics
                    .clone();
                self.fill_orders(
                    &mut block_building_helper,
                    &mut block_orders,
                    |sim_order| {
                        block_infos
                            .iter()
                            .any(|block_info| block_info.contains_order(&sim_order.order))
                    },
                    build_start,
                    self.config
                        .pre_filtered_build_duration_deadline()
                        .map(|d| build_start.elapsed() + d),
                )?;
                let considered_stats = block_building_helper
                    .built_block_trace()
                    .considered_orders_statistics
                    .clone()
                    - base_considered_orders_statistics;
                let failed_stats = block_building_helper
                    .built_block_trace()
                    .failed_orders_statistics
                    .clone()
                    - base_failed_orders_statistics;
                add_ordering_builder_pre_filtered_stage_stats(
                    self.builder_name.as_str(),
                    OrderInclusionRatio::new_from_failed(
                        considered_stats.total(),
                        failed_stats.total(),
                    ),
                );
                block_building_helper.set_filtered_build_statistics(considered_stats, failed_stats);
            }
        }
        block_building_helper.set_trace_fill_time(build_start.elapsed());

        Ok(Box::new(block_building_helper))
    }

    fn fill_orders<OrderPriorityType: OrderPriority, OrderFilter: Fn(&SimulatedOrder) -> bool>(
        &mut self,
        block_building_helper: &mut dyn BlockBuildingHelper,
        block_orders: &mut PrioritizedOrderStore<OrderPriorityType>,
        order_filter: OrderFilter,
        build_start: Instant,
        deadline: Option<Duration>,
    ) -> eyre::Result<()> {
        // @Perf when gas left is too low we should break.
        while let Some(sim_order) = block_orders.pop_order() {
            // @Todo we drop such bundles instead of failing simulation for them
            // because share bundle merging depends on allowing no txs bundles into the block
            if sim_order.sim_value.gas_used() == 0 || !order_filter(&sim_order) {
                continue;
            }

            if let Some(deadline) = deadline {
                if build_start.elapsed() > deadline {
                    break;
                }
            }
            mark_builder_considers_order(
                sim_order.id(),
                &block_building_helper.built_block_trace().orders_closed_at,
                block_building_helper.builder_name(),
            );
            let start_time = Instant::now();
            let commit_result = block_building_helper.commit_order(
                &mut self.local_ctx,
                &sim_order,
                &|sim_result| {
                    if !sim_order.order.metadata().is_system {
                        simulation_too_low::<OrderPriorityType>(&sim_order.sim_value, sim_result)
                    } else {
                        Ok(())
                    }
                },
            )?;
            let order_commit_time = start_time.elapsed();
            let mut gas_used = 0;
            let mut execution_error = None;
            let mut reinserted = false;
            let success = commit_result.is_ok();
            match commit_result {
                Ok(res) => {
                    gas_used = res.space_used.gas;
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
                        if order_is_worth_executing(inplace).is_ok() {
                            // try to reinsert order into the map
                            let order_attempts =
                                self.order_attempts.entry(sim_order.id()).or_insert(0);
                            if *order_attempts < self.config.failed_order_retries {
                                let mut new_order = (*sim_order).clone();
                                new_order.sim_value = inplace.clone();
                                block_orders.insert_order(Arc::new(new_order));
                                *order_attempts += 1;
                                reinserted = true;
                            }
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
pub struct OrderingBuildingAlgorithm<OrderPriorityType> {
    config: OrderingBuilderConfig,
    name: String,
    max_order_execution_duration_warning: Option<Duration>,
    /// The ordering priority type used to sort simulated orders.
    order_priority: PhantomData<OrderPriorityType>,
}

impl<OrderPriorityType> OrderingBuildingAlgorithm<OrderPriorityType> {
    pub fn new(
        config: OrderingBuilderConfig,
        max_order_execution_duration_warning: Option<Duration>,
        name: String,
    ) -> Self {
        Self {
            config,
            name,
            max_order_execution_duration_warning,
            order_priority: PhantomData,
        }
    }
}

impl<P, OrderPriorityType> BlockBuildingAlgorithm<P>
    for OrderingBuildingAlgorithm<OrderPriorityType>
where
    P: StateProviderFactory + Clone + 'static,
    OrderPriorityType: OrderPriority,
{
    fn name(&self) -> String {
        self.name.clone()
    }

    fn build_blocks(&self, input: BlockBuildingAlgorithmInput<P>) {
        let live_input = LiveBuilderInput {
            provider: input.provider,
            ctx: input.ctx.clone(),
            input: input.input,
            sink: input.sink,
            builder_name: self.name.clone(),
            cancel: input.cancel,
            built_block_cache: input.built_block_cache,
            built_block_id_source: input.built_block_id_source,
            max_order_execution_duration_warning: self.max_order_execution_duration_warning,
        };
        run_ordering_builder::<P, OrderPriorityType>(live_input, &self.config);
    }
}

// Check that new simulation results during block building are not much lower (defined by OrderPriority) than the top-of-block simulation results
// @Opt is large err OK here
#[allow(clippy::result_large_err)]
fn simulation_too_low<OrderPriorityType: OrderPriority>(
    original_sim_result: &SimValue,
    new_sim_result: &SimValue,
) -> Result<(), ExecutionError> {
    if OrderPriorityType::simulation_too_low(original_sim_result, new_sim_result) {
        Err(ExecutionError::LowerInsertedValue {
            before: original_sim_result.clone(),
            inplace: new_sim_result.clone(),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::building::order_priority::{
        FullProfitInfoGetter, OrderMaxProfitPriority, OrderMevGasPricePriority,
    };
    use alloy_primitives::U256;

    #[test]
    fn test_simulation_too_low_max_profit() {
        let sim_result = &SimValue::new_test_no_gas(U256::from(100), U256::from(0));
        let inplace_sim_result = &SimValue::new_test_no_gas(U256::from(94), U256::from(0));

        // Lower than 95% of the original value
        assert!(
            simulation_too_low::<OrderMaxProfitPriority::<FullProfitInfoGetter>>(
                sim_result,
                inplace_sim_result
            )
            .is_err()
        );

        // Equal to original value
        let inplace_sim_result = &SimValue::new_test_no_gas(U256::from(100), U256::from(0));
        assert!(
            simulation_too_low::<OrderMaxProfitPriority::<FullProfitInfoGetter>>(
                sim_result,
                inplace_sim_result
            )
            .is_ok()
        );

        // Higher than original value
        let inplace_sim_result = &SimValue::new_test_no_gas(U256::from(105), U256::from(0));
        assert!(
            simulation_too_low::<OrderMaxProfitPriority::<FullProfitInfoGetter>>(
                sim_result,
                inplace_sim_result
            )
            .is_ok()
        );
    }

    #[test]
    fn test_simulation_too_low_mev_gas_price() {
        let sim_result = &SimValue::new_test_no_gas(U256::from(0), U256::from(100));
        // Lower than 95% of the original value
        let inplace_sim_result = &SimValue::new_test_no_gas(U256::from(0), U256::from(94));

        assert!(
            simulation_too_low::<OrderMevGasPricePriority::<FullProfitInfoGetter>>(
                sim_result,
                inplace_sim_result
            )
            .is_err()
        );

        // Equal to original value
        let inplace_sim_result = &SimValue::new_test_no_gas(U256::from(0), U256::from(100));
        assert!(
            simulation_too_low::<OrderMevGasPricePriority::<FullProfitInfoGetter>>(
                sim_result,
                inplace_sim_result
            )
            .is_ok()
        );

        // Higher than original value
        let inplace_sim_result = &SimValue::new_test_no_gas(U256::from(0), U256::from(105));
        assert!(
            simulation_too_low::<OrderMevGasPricePriority::<FullProfitInfoGetter>>(
                sim_result,
                inplace_sim_result
            )
            .is_ok()
        );
    }
}
