use super::{
    results_aggregator::BestResults, ConflictGroup, GroupId, ParallelBuilderConfig,
    ResolutionResult,
};
use ahash::HashMap;
use alloy_primitives::utils::format_ether;
use reth_provider::StateProvider;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{info_span, trace};

use crate::{
    building::{
        builders::{
            block_building_helper::{
                BiddableUnfinishedBlock, BlockBuildingHelper, BlockBuildingHelperFromProvider,
            },
            handle_building_error, BuiltBlockIdSource,
        },
        BlockBuildingContext, ThreadBlockBuildingContext,
    },
    live_builder::block_output::unfinished_block_processing::UnfinishedBuiltBlocksInput,
    telemetry::mark_builder_considers_order,
    utils::elapsed_ms,
};
use rbuilder_primitives::order_statistics::OrderStatistics;

/// Assembles block building results from the best orderings of order groups.
pub struct BlockBuildingResultAssembler {
    state: Arc<dyn StateProvider>,
    ctx: BlockBuildingContext,
    pub local_ctx: ThreadBlockBuildingContext,
    cancellation_token: CancellationToken,
    discard_txs: bool,
    builder_name: String,
    sink: Option<UnfinishedBuiltBlocksInput>,
    best_results: Arc<BestResults>,
    run_id: u64,
    last_version: Option<u64>,
    built_block_id_source: Arc<BuiltBlockIdSource>,
    max_order_execution_duration_warning: Option<Duration>,
}

impl BlockBuildingResultAssembler {
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
        best_results: Arc<BestResults>,
        state: Arc<dyn StateProvider>,
        ctx: BlockBuildingContext,
        cancellation_token: CancellationToken,
        builder_name: String,
        sink: Option<UnfinishedBuiltBlocksInput>,
        built_block_id_source: Arc<BuiltBlockIdSource>,
        max_order_execution_duration_warning: Option<Duration>,
    ) -> Self {
        Self {
            state,
            ctx,
            local_ctx: Default::default(),
            cancellation_token,
            discard_txs: config.discard_txs,
            builder_name,
            sink,
            best_results,
            run_id: 0,
            last_version: None,
            built_block_id_source,
            max_order_execution_duration_warning,
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
                if !self.try_build_block(orders_closed_at) {
                    break;
                }
            }
        }
        trace!(
            "Parallel builder run id {}: Block building result assembler run finished",
            self.run_id
        );
    }

    /// Attempts to build a new block if not already building. Returns if block building should continue.
    ///
    /// # Arguments
    ///
    /// * `orders_closed_at` - The timestamp when orders were closed.
    fn try_build_block(&mut self, orders_closed_at: OffsetDateTime) -> bool {
        let time_start = Instant::now();

        let current_best_results = self.best_results.clone();
        let (mut best_orderings_per_group, version) =
            current_best_results.get_results_and_version();

        // Check if version has incremented
        if let Some(last_version) = self.last_version {
            if version == last_version {
                return true;
            }
        }
        self.last_version = Some(version);

        trace!(
            "Parallel builder run id {}: Attempting to build block with results version {}",
            self.run_id,
            version
        );

        if best_orderings_per_group.is_empty() {
            return true;
        }

        match self.build_new_block(&mut best_orderings_per_group, orders_closed_at) {
            Ok(new_block) => {
                if let Ok(value) = new_block.true_block_value() {
                    trace!(
                        run_id = self.run_id,
                        version = version,
                        time_ms = elapsed_ms(time_start),
                        profit = format_ether(value),
                        "Parallel builder built new block",
                    );

                    if let Some(sink) = &self.sink {
                        if let Ok(new_block) = BiddableUnfinishedBlock::new(new_block) {
                            sink.new_block(new_block);
                        }
                    }
                }
            }
            Err(err) => {
                let _span = info_span!("Parallel builder failed to build new block",run_id = self.run_id,version = version,err=?err).entered();
                if !handle_building_error(err, self.ctx.payload_id) {
                    return false;
                }
            }
        }
        self.run_id += 1;
        true
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

        let mut block_building_helper = BlockBuildingHelperFromProvider::new(
            self.built_block_id_source.get_new_id(),
            self.state.clone(),
            self.ctx.clone(),
            &mut self.local_ctx,
            self.builder_name.clone(),
            self.discard_txs,
            OrderStatistics::default(),
            self.cancellation_token.clone(),
            self.max_order_execution_duration_warning,
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

                mark_builder_considers_order(
                    sim_order.id(),
                    &block_building_helper.built_block_trace().orders_closed_at,
                    block_building_helper.builder_name(),
                );
                let start_time = Instant::now();
                let commit_result =
                    block_building_helper
                        .commit_order(&mut self.local_ctx, sim_order, &|_| Ok(()))?;
                let order_commit_time = start_time.elapsed();

                let mut gas_used = 0;
                let mut execution_error = None;
                let success = commit_result.is_ok();
                match commit_result {
                    Ok(res) => {
                        gas_used = res.space_used.gas;
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
        Ok(Box::new(block_building_helper))
    }

    pub fn build_backtest_block(
        &mut self,
        best_results: HashMap<GroupId, (ResolutionResult, ConflictGroup)>,
        orders_closed_at: OffsetDateTime,
    ) -> eyre::Result<Box<dyn BlockBuildingHelper>> {
        let mut block_building_helper = BlockBuildingHelperFromProvider::new(
            self.built_block_id_source.get_new_id(),
            self.state.clone(),
            self.ctx.clone(),
            &mut self.local_ctx,
            String::from("backtest_builder"),
            self.discard_txs,
            OrderStatistics::default(),
            CancellationToken::new(),
            self.max_order_execution_duration_warning,
        )?;

        block_building_helper.set_trace_orders_closed_at(orders_closed_at);

        let mut best_orderings_per_group: Vec<(ResolutionResult, ConflictGroup)> =
            best_results.into_values().collect();

        // Sort groups by total profit in descending order
        best_orderings_per_group.sort_by(|(a_ordering, _), (b_ordering, _)| {
            b_ordering.total_profit.cmp(&a_ordering.total_profit)
        });

        let build_start = Instant::now();

        for (sequence_of_orders, order_group) in best_orderings_per_group.iter_mut() {
            for (order_idx, _) in sequence_of_orders.sequence_of_orders.iter() {
                let sim_order = &order_group.orders[*order_idx];

                let commit_result =
                    block_building_helper
                        .commit_order(&mut self.local_ctx, sim_order, &|_| Ok(()))?;

                match commit_result {
                    Ok(res) => {
                        tracing::trace!(
                            order_id = ?sim_order.id(),
                            success = true,
                            gas_used = res.space_used.gas,
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
}
