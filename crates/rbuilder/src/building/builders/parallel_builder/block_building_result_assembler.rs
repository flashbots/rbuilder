use super::{
    results_aggregator::BestResults, ConflictGroup, GroupId, ParallelBuilderConfig,
    ResolutionResult,
};
use ahash::HashMap;
use alloy_primitives::utils::format_ether;
use reth::tasks::pool::BlockingTaskPool;
use reth_db::Database;
use reth_payload_builder::database::CachedReads;
use reth_provider::{DatabaseProviderFactory, StateProviderFactory};
use std::{marker::PhantomData, sync::Arc, time::Instant};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{trace, warn};

use crate::{
    building::{
        builders::{
            block_building_helper::{BlockBuildingHelper, BlockBuildingHelperFromProvider},
            UnfinishedBlockBuildingSink,
        },
        BlockBuildingContext,
    },
    roothash::RootHashConfig,
};

/// Assembles block building results from the best orderings of order groups.
pub struct BlockBuildingResultAssembler<P, DB> {
    provider: P,
    root_hash_task_pool: BlockingTaskPool,
    ctx: BlockBuildingContext,
    cancellation_token: CancellationToken,
    cached_reads: Option<CachedReads>,
    discard_txs: bool,
    coinbase_payment: bool,
    can_use_suggested_fee_recipient_as_coinbase: bool,
    root_hash_config: RootHashConfig,
    builder_name: String,
    sink: Option<Arc<dyn UnfinishedBlockBuildingSink>>,
    best_results: Arc<BestResults>,
    run_id: u64,
    last_version: Option<u64>,
    phantom: PhantomData<DB>,
}

impl<P, DB> BlockBuildingResultAssembler<P, DB>
where
    DB: Database + Clone + 'static,
    P: DatabaseProviderFactory<DB> + StateProviderFactory + Clone + 'static,
{
    /// Creates a new `BlockBuildingResultAssembler`.
    ///
    /// # Arguments
    ///
    /// * `input` - The live builder input containing necessary components.
    /// * `config` - The configuration for the Parallel builder.
    /// * `build_trigger_receiver` - A receiver for build trigger signals.
    /// * `best_results` - A shared map of the best results for each group.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &ParallelBuilderConfig,
        root_hash_config: RootHashConfig,
        best_results: Arc<BestResults>,
        provider: P,
        root_hash_task_pool: BlockingTaskPool,
        ctx: BlockBuildingContext,
        cancellation_token: CancellationToken,
        builder_name: String,
        can_use_suggested_fee_recipient_as_coinbase: bool,
        sink: Option<Arc<dyn UnfinishedBlockBuildingSink>>,
    ) -> Self {
        Self {
            provider,
            root_hash_task_pool,
            ctx,
            cancellation_token,
            cached_reads: None,
            discard_txs: config.discard_txs,
            coinbase_payment: config.coinbase_payment,
            can_use_suggested_fee_recipient_as_coinbase,
            root_hash_config,
            builder_name,
            sink,
            best_results,
            run_id: 0,
            last_version: None,
            phantom: PhantomData,
        }
    }

    /// Runs the block building process continuously.
    ///
    /// # Arguments
    ///
    /// * `cancel_token` - A token to signal cancellation of the process.
    pub fn run(&mut self, cancel_token: CancellationToken) {
        trace!(
            "Parallel builder run id {}: Block building result assembler run started",
            self.run_id
        );
        // To-do: decide if we want to trigger builds here or just build in a continuous loop
        loop {
            if cancel_token.is_cancelled() {
                break;
            }
            if self.best_results.get_number_of_orders() > 0 {
                let orders_closed_at = OffsetDateTime::now_utc();
                self.try_build_block(orders_closed_at);
            }
        }
        trace!(
            "Parallel builder run id {}: Block building result assembler run finished",
            self.run_id
        );
    }

    /// Attempts to build a new block if not already building.
    ///
    /// # Arguments
    ///
    /// * `orders_closed_at` - The timestamp when orders were closed.
    fn try_build_block(&mut self, orders_closed_at: OffsetDateTime) {
        let time_start = Instant::now();

        let current_best_results = self.best_results.clone();
        let (mut best_orderings_per_group, version) =
            current_best_results.get_results_and_version();

        // Check if version has incremented
        if let Some(last_version) = self.last_version {
            if version == last_version {
                return;
            }
        }
        self.last_version = Some(version);

        trace!(
            "Parallel builder run id {}: Attempting to build block with results version {}",
            self.run_id,
            version
        );

        if best_orderings_per_group.is_empty() {
            return;
        }

        match self.build_new_block(&mut best_orderings_per_group, orders_closed_at) {
            Ok(new_block) => {
                if let Ok(value) = new_block.true_block_value() {
                    trace!(
                        "Parallel builder run id {}: Built new block with results version {:?} and profit: {:?} in {:?} ms",
                        self.run_id,
                        version,
                        format_ether(value),
                        time_start.elapsed().as_millis()
                    );

                    if new_block.built_block_trace().got_no_signer_error {
                        self.can_use_suggested_fee_recipient_as_coinbase = false;
                    }

                    if let Some(sink) = &self.sink {
                        sink.new_block(new_block);
                    }
                }
            }
            Err(e) => {
                warn!("Parallel builder run id {}: Failed to build new block with results version {:?}: {:?}", self.run_id, version, e);
            }
        }
        self.run_id += 1;
    }

    /// Builds a new block using the best results from each group.
    ///
    /// # Arguments
    ///
    /// * `best_results` - The current best results for each group.
    /// * `orders_closed_at` - The timestamp when orders were closed.
    ///
    /// # Returns
    ///
    /// A Result containing the new block building helper or an error.
    pub fn build_new_block(
        &mut self,
        best_orderings_per_group: &mut [(ResolutionResult, ConflictGroup)],
        orders_closed_at: OffsetDateTime,
    ) -> eyre::Result<Box<dyn BlockBuildingHelper>> {
        let build_start = Instant::now();

        let use_suggested_fee_recipient_as_coinbase = self.coinbase_payment
            && !self.contains_refunds(best_orderings_per_group)
            && self.can_use_suggested_fee_recipient_as_coinbase;

        // Create a new ctx to remove builder_signer if necessary
        let mut ctx = self.ctx.clone();
        if use_suggested_fee_recipient_as_coinbase {
            ctx.modify_use_suggested_fee_recipient_as_coinbase();
        }

        let mut block_building_helper = BlockBuildingHelperFromProvider::new(
            self.provider.clone(),
            self.root_hash_task_pool.clone(),
            self.root_hash_config.clone(),
            ctx,
            self.cached_reads.clone(),
            self.builder_name.clone(),
            self.discard_txs,
            None,
            self.cancellation_token.clone(),
        )?;
        block_building_helper.set_trace_orders_closed_at(orders_closed_at);

        // Sort groups by total profit in descending order
        best_orderings_per_group.sort_by(|(a_ordering, _), (b_ordering, _)| {
            b_ordering.total_profit.cmp(&a_ordering.total_profit)
        });

        loop {
            if self.cancellation_token.is_cancelled() {
                break;
            }

            // Find the first non-empty group
            let group_with_orders = best_orderings_per_group
                .iter_mut()
                .find(|(sequence_of_orders, _)| !sequence_of_orders.sequence_of_orders.is_empty());

            if let Some((sequence_of_orders, order_group)) = group_with_orders {
                // Get the next order from this group
                let (order_idx, _) = sequence_of_orders.sequence_of_orders.remove(0);
                let sim_order = &order_group.orders[order_idx];

                let start_time = Instant::now();
                let commit_result = block_building_helper.commit_order(sim_order)?;
                let order_commit_time = start_time.elapsed();

                let mut gas_used = 0;
                let mut execution_error = None;
                let success = commit_result.is_ok();
                match commit_result {
                    Ok(res) => {
                        gas_used = res.gas_used;
                    }
                    Err(err) => execution_error = Some(err),
                }
                trace!(
                    order_id = ?sim_order.id(),
                    success,
                    order_commit_time_mus = order_commit_time.as_micros(),
                    gas_used,
                    ?execution_error,
                    "Executed order"
                );
            } else {
                // No more orders in any group
                break;
            }
        }
        block_building_helper.set_trace_fill_time(build_start.elapsed());
        self.cached_reads = Some(block_building_helper.clone_cached_reads());
        Ok(Box::new(block_building_helper))
    }

    pub fn build_backtest_block(
        &self,
        best_results: HashMap<GroupId, (ResolutionResult, ConflictGroup)>,
        orders_closed_at: OffsetDateTime,
    ) -> eyre::Result<Box<dyn BlockBuildingHelper>> {
        let mut block_building_helper = BlockBuildingHelperFromProvider::new(
            self.provider.clone(),
            self.root_hash_task_pool.clone(),
            self.root_hash_config.clone(), // Adjust as needed for backtest
            self.ctx.clone(),
            None, // No cached reads for backtest start
            String::from("backtest_builder"),
            self.discard_txs,
            None,
            CancellationToken::new(),
        )?;

        block_building_helper.set_trace_orders_closed_at(orders_closed_at);

        let mut best_orderings_per_group: Vec<(ResolutionResult, ConflictGroup)> =
            best_results.into_values().collect();

        // Sort groups by total profit in descending order
        best_orderings_per_group.sort_by(|(a_ordering, _), (b_ordering, _)| {
            b_ordering.total_profit.cmp(&a_ordering.total_profit)
        });

        let use_suggested_fee_recipient_as_coinbase =
            self.coinbase_payment && !self.contains_refunds(&best_orderings_per_group);

        // Modify ctx if necessary
        let mut ctx = self.ctx.clone();
        if use_suggested_fee_recipient_as_coinbase {
            ctx.modify_use_suggested_fee_recipient_as_coinbase();
        }

        let build_start = Instant::now();

        for (sequence_of_orders, order_group) in best_orderings_per_group.iter_mut() {
            for (order_idx, _) in sequence_of_orders.sequence_of_orders.iter() {
                let sim_order = &order_group.orders[*order_idx];

                let commit_result = block_building_helper.commit_order(sim_order)?;

                match commit_result {
                    Ok(res) => {
                        tracing::trace!(
                            order_id = ?sim_order.id(),
                            success = true,
                            gas_used = res.gas_used,
                            "Executed order in backtest"
                        );
                    }
                    Err(err) => {
                        tracing::trace!(
                            order_id = ?sim_order.id(),
                            success = false,
                            error = ?err,
                            "Failed to execute order in backtest"
                        );
                    }
                }
            }
        }

        block_building_helper.set_trace_fill_time(build_start.elapsed());

        Ok(Box::new(block_building_helper))
    }

    /// Checks if any of the orders in the given orderings contain refunds.
    ///
    /// # Arguments
    ///
    /// * `orderings` - A slice of tuples containing group orderings and order groups.
    ///
    /// # Returns
    ///
    /// `true` if any order contains refunds, `false` otherwise.
    fn contains_refunds(&self, orderings: &[(ResolutionResult, ConflictGroup)]) -> bool {
        orderings.iter().any(|(sequence_of_orders, order_group)| {
            sequence_of_orders
                .sequence_of_orders
                .iter()
                .any(|(order_idx, _)| {
                    !order_group.orders[*order_idx]
                        .sim_value
                        .paid_kickbacks
                        .is_empty()
                })
        })
    }
}
