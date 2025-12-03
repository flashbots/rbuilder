pub mod block_building_result_assembler;
pub mod conflict_resolvers;
pub mod conflict_resolving_pool;
pub mod conflict_task_generator;
pub mod groups;
pub mod order_intake_store;
pub mod results_aggregator;
pub mod simulation_cache;
pub mod task;
use alloy_primitives::I256;
pub use groups::*;

use ahash::HashMap;
use conflict_resolving_pool::{ConflictResolvingPool, TaskQueue};
use conflict_task_generator::ConflictTaskGenerator;
use crossbeam::queue::SegQueue;
use eyre::Result;
use itertools::Itertools;
use results_aggregator::BestResults;
use reth_provider::StateProvider;
use serde::Deserialize;
use simulation_cache::SharedSimulationCache;
use std::{
    sync::{mpsc as std_mpsc, Arc},
    thread,
    time::{Duration, Instant},
};
use task::*;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{error, trace};

use crate::{
    building::builders::{
        BacktestSimulateBlockInput, Block, BlockBuildingAlgorithm, BlockBuildingAlgorithmInput,
        BuiltBlockIdSource, LiveBuilderInput,
    },
    provider::StateProviderFactory,
    utils::elapsed_ms,
};

use self::{
    block_building_result_assembler::BlockBuildingResultAssembler,
    order_intake_store::OrderIntakeStore, results_aggregator::ResultsAggregator,
};

pub type GroupId = usize;
pub type ConflictResolutionResultPerGroup = (GroupId, (ResolutionResult, ConflictGroup));

/// ParallelBuilderConfig configures parallel builder.
/// * `num_threads` - number of threads to use for merging.
/// * `merge_wait_time_ms` - time to wait for merging to finish before consuming new orders.
/// * `safe_sorting_only` - Will only use sort modes that don't risk breaking the "best refund for user" since we don't megabundle the bundles (only the sbundles).
///   This flag is just to test the algo until we solve every issue.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ParallelBuilderConfig {
    pub discard_txs: bool,
    pub num_threads: usize,
    pub safe_sorting_only: bool,
}

fn get_communication_channels() -> (
    std_mpsc::Sender<ConflictResolutionResultPerGroup>,
    std_mpsc::Receiver<ConflictResolutionResultPerGroup>,
) {
    std_mpsc::channel()
}

fn get_shared_data_structures() -> (Arc<BestResults>, TaskQueue) {
    let best_results = Arc::new(BestResults::new());
    let task_queue = Arc::new(SegQueue::new());
    (best_results, task_queue)
}

struct ParallelBuilder<P> {
    order_intake_consumer: OrderIntakeStore,
    conflict_finder: ConflictFinder,
    conflict_task_generator: ConflictTaskGenerator,
    conflict_resolving_pool: ConflictResolvingPool<P>,
    results_aggregator: ResultsAggregator,
    block_building_result_assembler: BlockBuildingResultAssembler,
}

impl<P> ParallelBuilder<P>
where
    P: StateProviderFactory + Clone + 'static,
{
    /// Creates a ParallelBuilder.
    /// Sets up the various components and communication channels.
    pub fn try_new(
        input: LiveBuilderInput<P>,
        config: &ParallelBuilderConfig,
    ) -> eyre::Result<Self> {
        let (group_result_sender, group_result_receiver) = get_communication_channels();
        let group_result_sender_for_task_generator = group_result_sender.clone();

        let (best_results, task_queue) = get_shared_data_structures();

        let simulation_cache = Arc::new(SharedSimulationCache::new());

        let conflict_finder = ConflictFinder::new();

        let conflict_task_generator = ConflictTaskGenerator::new(
            config.safe_sorting_only,
            Arc::clone(&task_queue),
            group_result_sender_for_task_generator,
        );

        let conflict_resolving_pool = ConflictResolvingPool::new(
            config.num_threads,
            Arc::clone(&task_queue),
            config.safe_sorting_only,
            group_result_sender,
            input.cancel.clone(),
            input.ctx.clone(),
            input.provider.clone(),
            Arc::clone(&simulation_cache),
        );

        let results_aggregator =
            ResultsAggregator::new(group_result_receiver, Arc::clone(&best_results));

        let block_state = input
            .provider
            .history_by_block_hash(input.ctx.attributes.parent)?
            .into();

        let block_building_result_assembler = BlockBuildingResultAssembler::new(
            config,
            Arc::clone(&best_results),
            block_state,
            input.ctx.clone(),
            input.cancel.clone(),
            input.builder_name.clone(),
            Some(input.sink.clone()),
            input.built_block_id_source.clone(),
            input.max_order_execution_duration_warning,
        );

        let order_intake_consumer = OrderIntakeStore::new(input.input);

        Ok(Self {
            order_intake_consumer,
            conflict_finder,
            conflict_task_generator,
            conflict_resolving_pool,
            results_aggregator,
            block_building_result_assembler,
        })
    }

    /// Initializes the orders in the cached groups.
    fn initialize_orders(&mut self) {
        let initial_orders = self.order_intake_consumer.get_orders();
        trace!("Initializing with {} orders", initial_orders.len());
        self.conflict_finder.add_orders(initial_orders);
    }
}

/// Runs the parallel builder algorithm to construct blocks from incoming orders.
///
/// This function implements a continuous block building process that:
/// 1. Consumes orders from an intake store.
/// 2. Identifies conflict groups among the orders.
/// 3. Manages conflicts and attempts to resolve them.
/// 4. Builds blocks from the best results of conflict resolution.
///
/// The process involves several key components:
/// - [OrderIntakeStore]: Provides a continuous stream of incoming orders.
/// - [ConflictFinder]: Identifies and manages conflict groups among orders.
/// - [ConflictTaskGenerator]: Decides which conflicts to attempt to resolve and what priority.
/// - [ConflictResolvingPool]: A pool of workers that resolve conflicts between orders, producing "results" which are resolved conflicts.
/// - [ResultsAggregator]: Collects results from workers and initiates block building.
/// - [BlockBuildingResultAssembler]: Builds blocks from the best results collected.
///
/// The function runs in a loop, continuously processing new orders and building blocks
/// until cancellation is requested. It uses separate processes for
/// 1. Identifying conflicts and processing which conflicts to attempt to resolve in what priority
/// 2. Resolving conflicts
/// 3. Block building given conflict resolution results
///
/// By separating these processes we can continuously take in new flow, triage that flow intelligently, and build blocks continuously with the best results.
///
/// # Arguments
/// * `input`: LiveBuilderInput containing necessary context and resources for block building.
/// * `config`: Configuration parameters for the parallel builder.
///
/// # Type Parameters
/// * `DB`: The database type, which must implement Database, Clone, and have a static lifetime.
pub fn run_parallel_builder<P>(input: LiveBuilderInput<P>, config: &ParallelBuilderConfig)
where
    P: StateProviderFactory + Clone + 'static,
{
    let cancel_for_results_aggregator = input.cancel.clone();
    let cancel_for_block_building_result_assembler = input.cancel.clone();
    let cancel_for_process_orders_loop = input.cancel.clone();

    let mut builder = match ParallelBuilder::try_new(input, config) {
        Ok(builder) => builder,
        Err(err) => {
            error!(?err, "Failed to create parallel builder, cancelling");
            return;
        }
    };
    builder.initialize_orders();

    // Start task processing
    match builder.conflict_resolving_pool.start() {
        Ok(()) => {}
        Err(err) => {
            error!(
                ?err,
                "Failed to start parallel builder conflict_resolving_pool, cancelling"
            );
            return;
        }
    }

    // Process that collects conflict resolution results from workers and triggers block building
    tokio::spawn(async move {
        builder
            .results_aggregator
            .run(cancel_for_results_aggregator)
            .await;
    });

    // Process that builds blocks from the best conflict resolution results
    thread::spawn(move || {
        builder
            .block_building_result_assembler
            .run(cancel_for_block_building_result_assembler);
    });

    // Process that consumes orders from the intake store, updates the cached groups, and triggers new conflict resolution tasks for the worker pool
    run_order_intake(
        &cancel_for_process_orders_loop,
        &mut builder.order_intake_consumer,
        &mut builder.conflict_finder,
        &mut builder.conflict_task_generator,
    );
}

fn run_order_intake(
    cancel_token: &CancellationToken,
    order_intake_consumer: &mut OrderIntakeStore,
    conflict_finder: &mut ConflictFinder,
    conflict_task_generator: &mut ConflictTaskGenerator,
) {
    'building: loop {
        if cancel_token.is_cancelled() {
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

        let new_orders = order_intake_consumer.try_drain_new_orders_if_no_cancellations();

        // We can update conflict_finder if we have ONLY adds
        if let Some(new_orders) = new_orders {
            if !new_orders.is_empty() {
                let time_start = Instant::now();
                let len = new_orders.len();
                conflict_finder.add_orders(new_orders);
                trace!(
                    new_orders_count = len,
                    groups_count = conflict_finder.get_order_groups().len(),
                    time_taken_ms = %elapsed_ms(time_start),
                    "Order intake: added new orders and processing groups"
                );
                conflict_task_generator.process_groups(conflict_finder.get_order_groups());
            }
        }
    }
}

pub fn parallel_build_backtest<P>(
    input: BacktestSimulateBlockInput<'_, P>,
    config: ParallelBuilderConfig,
) -> Result<Block>
where
    P: StateProviderFactory + Clone + 'static,
{
    let start_time = Instant::now();

    // Initialization stage
    let init_start = Instant::now();
    let (best_results, task_queue) = get_shared_data_structures();

    let (group_result_sender, _) = get_communication_channels();

    let mut conflict_finder = ConflictFinder::new();

    let sorted_orders = {
        let mut orders = input.sim_orders.clone();
        orders.sort_by_key(|o| o.order.id());
        orders
    };

    conflict_finder.add_orders(sorted_orders);
    let simulation_cache = Arc::new(SharedSimulationCache::new());
    let init_duration = init_start.elapsed();

    // Worker pool and conflict manager creation
    let setup_start = Instant::now();

    let mut conflict_resolving_pool = ConflictResolvingPool::new(
        config.num_threads,
        Arc::clone(&task_queue),
        config.safe_sorting_only,
        group_result_sender,
        CancellationToken::new(),
        input.ctx.clone(),
        input.provider.clone(),
        Arc::clone(&simulation_cache),
    );

    let setup_duration = setup_start.elapsed();

    let block_state: Arc<dyn StateProvider> = input
        .provider
        .history_by_block_hash(input.ctx.attributes.parent)?
        .into();

    // Group processing
    let processing_start = Instant::now();
    let groups = conflict_finder.get_order_groups();
    let results = conflict_resolving_pool.process_groups_backtest(
        groups,
        &input.ctx,
        block_state.clone(),
        Arc::clone(&simulation_cache),
    );
    let processing_duration = processing_start.elapsed();

    // Block building result assembler creation
    let assembler_start = Instant::now();
    let mut block_building_result_assembler = BlockBuildingResultAssembler::new(
        &config,
        Arc::clone(&best_results),
        block_state.clone(),
        input.ctx.clone(),
        CancellationToken::new(),
        String::from("backtest_builder"),
        None,
        Arc::new(BuiltBlockIdSource::new()),
        None,
    );
    let assembler_duration = assembler_start.elapsed();

    // Best results collection
    let collection_start = Instant::now();
    let best_results: HashMap<GroupId, (ResolutionResult, ConflictGroup)> = results
        .into_iter()
        .sorted_by(|a, b| b.1 .0.total_profit.cmp(&a.1 .0.total_profit))
        .into_group_map_by(|(group_id, _)| *group_id)
        .into_iter()
        .map(|(group_id, mut group_results)| (group_id, group_results.remove(0).1))
        .collect();
    let collection_duration = collection_start.elapsed();

    // Block building
    let building_start = Instant::now();
    let mut block_building_helper = block_building_result_assembler
        .build_backtest_block(best_results, OffsetDateTime::now_utc())?;

    let payout_tx_value = block_building_helper.true_block_value()?;
    let finalize_block_result = block_building_helper.finalize_block(
        &mut block_building_result_assembler.local_ctx,
        payout_tx_value,
        I256::ZERO,
        None,
    )?;
    let building_duration = building_start.elapsed();
    let total_duration = start_time.elapsed();

    trace!("Initialization time: {:?}", init_duration);
    trace!("Setup time: {:?}", setup_duration);
    trace!("Group processing time: {:?}", processing_duration);
    trace!("Assembler creation time: {:?}", assembler_duration);
    trace!("Best results collection time: {:?}", collection_duration);
    trace!("Block building time: {:?}", building_duration);
    trace!("Total time taken: {:?}", total_duration);

    Ok(finalize_block_result.block)
}

#[derive(Debug)]
pub struct ParallelBuildingAlgorithm {
    config: ParallelBuilderConfig,
    max_order_execution_duration_warning: Option<Duration>,
    name: String,
}

impl ParallelBuildingAlgorithm {
    pub fn new(
        config: ParallelBuilderConfig,
        max_order_execution_duration_warning: Option<Duration>,
        name: String,
    ) -> Self {
        Self {
            config,
            max_order_execution_duration_warning,
            name,
        }
    }
}

impl<P> BlockBuildingAlgorithm<P> for ParallelBuildingAlgorithm
where
    P: StateProviderFactory + Clone + 'static,
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
        run_parallel_builder(live_input, &self.config);
    }
}
