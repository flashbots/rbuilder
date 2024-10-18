use alloy_primitives::utils::format_ether;
use crossbeam_queue::SegQueue;
use eyre::Result;
use rayon::{ThreadPool, ThreadPoolBuilder};
use reth::providers::ProviderFactory;
use reth_db::database::Database;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, trace, warn};

use crate::building::BlockBuildingContext;
use super::conflict_task_generator::get_tasks_for_group;
use super::ConflictResolutionResultPerGroup;
use super::TaskPriority;
use super::{
    conflict_resolvers::ResolverContext, simulation_cache::SharedSimulationCache, ConflictGroup,
    ConflictTask, GroupId, ResolutionResult,
};

pub type TaskQueue = Arc<SegQueue<ConflictTask>>;

pub struct ConflictResolvingPool<DB: Database + Clone + 'static> {
    task_queue: TaskQueue,
    thread_pool: ThreadPool,
    group_result_sender: std_mpsc::Sender<ConflictResolutionResultPerGroup>,
    cancellation_token: CancellationToken,
    ctx: BlockBuildingContext,
    provider_factory: ProviderFactory<DB>,
    simulation_cache: Arc<SharedSimulationCache>,
}

impl<DB: Database + Clone + 'static> ConflictResolvingPool<DB> {
    pub fn new(
        num_threads: usize,
        task_queue: TaskQueue,
        group_result_sender: std_mpsc::Sender<ConflictResolutionResultPerGroup>,
        cancellation_token: CancellationToken,
        ctx: BlockBuildingContext,
        provider_factory: ProviderFactory<DB>,
        simulation_cache: Arc<SharedSimulationCache>,
    ) -> Self {
        let thread_pool = ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build()
            .expect("Failed to build thread pool");

        Self {
            task_queue,
            thread_pool,
            group_result_sender,
            cancellation_token,
            ctx,
            provider_factory,
            simulation_cache,
        }
    }

    pub fn start(&self) {
        let task_queue = self.task_queue.clone();
        let cancellation_token = self.cancellation_token.clone();
        let provider_factory = self.provider_factory.clone();
        let group_result_sender = self.group_result_sender.clone();
        let simulation_cache = self.simulation_cache.clone();
        let ctx = self.ctx.clone();

        self.thread_pool.spawn(move || {
            while !cancellation_token.is_cancelled() {
                if let Some(task) = task_queue.pop() {
                    let task_start = Instant::now();
                    if let Ok((task_id, result)) = Self::process_task(
                        task,
                        &ctx,
                        &provider_factory,
                        cancellation_token.clone(),
                        Arc::clone(&simulation_cache),
                    ) {
                        match group_result_sender.send((task_id, result)) {
                            Ok(_) => {
                                trace!(
                                    task_id = %task_id,
                                    time_taken_ms = %task_start.elapsed().as_millis(),
                                    "Conflict resolving: successfully sent group result"
                                );
                            }
                            Err(err) => {
                                warn!(
                                    task_id = %task_id,
                                    error = ?err,
                                    time_taken_ms = %task_start.elapsed().as_millis(),
                                    "Conflict resolving: failed to send group result"
                                );
                            }
                        }
                    }
                }
            }
        });
    }

    pub fn process_task(
        task: ConflictTask,
        ctx: &BlockBuildingContext,
        provider_factory: &ProviderFactory<DB>,
        cancellation_token: CancellationToken,
        simulation_cache: Arc<SharedSimulationCache>,
    ) -> Result<(GroupId, (ResolutionResult, ConflictGroup))> {
        let mut merging_context = ResolverContext::new(
            provider_factory.clone(),
            ctx.clone(),
            cancellation_token,
            None,
            simulation_cache,
        );
        let task_id = task.group_idx;
        let task_group = task.group.clone();
        let task_algo = task.algorithm;

        match merging_context.run_conflict_task(task) {
            Ok(sequence_of_orders) => {
                info!(
                    task_type = ?task_algo,
                    group_id = task_id,
                    profit = format_ether(sequence_of_orders.total_profit),
                    order_count = sequence_of_orders.sequence_of_orders.len(),
                    "Successfully ran conflict task"
                );
                Ok((task_id, (sequence_of_orders, task_group)))
            }
            Err(err) => {
                warn!(
                    "Error running conflict task for group_idx {}: {:?}",
                    task_id, err
                );
                Err(err)
            }
        }
    }

    pub fn process_groups_backtest(
        &mut self,
        new_groups: Vec<ConflictGroup>,
        ctx: &BlockBuildingContext,
        provider_factory: &ProviderFactory<DB>,
        simulation_cache: Arc<SharedSimulationCache>,
    ) -> Vec<(GroupId, (ResolutionResult, ConflictGroup))> {
        let mut results = Vec::new();
        for new_group in new_groups {
            let tasks = get_tasks_for_group(&new_group, TaskPriority::High);
            for task in tasks {
                let simulation_cache = Arc::clone(&simulation_cache);
                let result = Self::process_task(
                    task,
                    ctx,
                    provider_factory,
                    CancellationToken::new(),
                    simulation_cache,
                );
                if let Ok(result) = result {
                    results.push(result);
                }
            }
        }
        results
    }
}
