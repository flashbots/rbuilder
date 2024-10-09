use crate::building::BlockBuildingContext;
use reth::providers::ProviderFactory;
use reth_db::database::Database;
use reth_payload_builder::database::CachedReads;
use std::{sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::{trace, warn};

use super::{
    merger::{MergeTask, MergeTaskCommand, MergingContext},
    OrderGroup,
};

/// `MergingPool` is a set of threads that try ordering for the given groups of orders.
pub struct MergingPool<DB> {
    provider_factory: ProviderFactory<DB>,
    ctx: BlockBuildingContext,
    num_threads: usize,
    global_cancellation_token: CancellationToken,
    cached_reads: Vec<CachedReads>,

    current_task_cancellaton_token: CancellationToken,
    thread_handles: Vec<std::thread::JoinHandle<CachedReads>>,
}

impl<DB: Database + Clone + 'static> MergingPool<DB> {
    pub fn new(
        provider_factory: ProviderFactory<DB>,
        ctx: BlockBuildingContext,
        num_threads: usize,
        global_cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            provider_factory,
            ctx,
            num_threads,
            global_cancellation_token,
            cached_reads: Vec::new(),
            current_task_cancellaton_token: CancellationToken::new(),
            thread_handles: Vec::new(),
        }
    }

    pub fn start_merging_tasks(&mut self, groups: Vec<OrderGroup>) -> eyre::Result<()> {
        self.current_task_cancellaton_token = self.global_cancellation_token.child_token();

        let queue = Arc::new(crossbeam_queue::ArrayQueue::new(10_000));
        for (group_idx, group) in groups.iter().enumerate() {
            if group.orders.len() == 1 {
                continue;
            } else if group.orders.len() <= 3 {
                let _ = queue.push(MergeTask {
                    group_idx,
                    command: MergeTaskCommand::AllPermutations,
                });
            } else {
                let _ = queue.push(MergeTask {
                    group_idx,
                    command: MergeTaskCommand::StaticOrdering {
                        extra_orderings: true,
                    },
                });
            }
        }
        // Here we fill queue of tasks with random ordering tasks for big groups.
        for i in 0..150 {
            for (group_idx, group) in groups.iter().enumerate() {
                if group.orders.len() > 3 {
                    let _ = queue.push(MergeTask {
                        group_idx,
                        command: MergeTaskCommand::RandomPermutations {
                            seed: group_idx as u64 + i,
                            count: 2,
                        },
                    });
                }
            }
        }

        for idx in 0..self.num_threads {
            let mut merging_context = MergingContext::new(
                self.provider_factory.clone(),
                self.ctx.clone(),
                groups.clone(),
                self.current_task_cancellaton_token.clone(),
                self.cached_reads.pop(),
            );

            let cancellation = self.current_task_cancellaton_token.clone();
            let queue = queue.clone();
            let handle = std::thread::Builder::new()
                .name(format!("merging-thread-{}", idx))
                .spawn(move || {
                    while let Some(task) = queue.pop() {
                        if cancellation.is_cancelled() {
                            break;
                        }

                        let start = Instant::now();
                        if let Err(err) = merging_context.run_merging_task(task.clone()) {
                            warn!("Error running merging task: {:?}", err);
                        }
                        trace!(
                            "Finished merging task: {:?}, elapsed: {:?}, len: {}",
                            task,
                            start.elapsed(),
                            merging_context.groups[task.group_idx].orders.len(),
                        );
                    }

                    merging_context.into_cached_reads()
                })?;

            self.thread_handles.push(handle);
        }

        Ok(())
    }

    pub fn stop_merging_threads(&mut self) -> eyre::Result<()> {
        self.current_task_cancellaton_token.cancel();
        for join_handle in self.thread_handles.drain(..) {
            let reads = join_handle
                .join()
                .map_err(|_err| eyre::eyre!("Error joining merging thread"))?;
            self.cached_reads.push(reads);
        }
        Ok(())
    }
}
