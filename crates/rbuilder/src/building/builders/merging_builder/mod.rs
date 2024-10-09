pub mod combinator;
pub mod groups;
pub mod merger;
pub mod merging_pool;
pub mod order_intake_store;
use combinator::CombinatorContext;
pub use groups::*;
use merger::{MergeTask, MergeTaskCommand, MergingContext};
use merging_pool::MergingPool;
use tracing::{error, trace};

use self::order_intake_store::OrderIntakeStore;

use crate::{
    building::builders::{
        handle_building_error, BacktestSimulateBlockInput, Block, BlockBuildingAlgorithm,
        BlockBuildingAlgorithmInput, LiveBuilderInput,
    },
    roothash::RootHashConfig,
};
use alloy_primitives::{utils::format_ether, Address};
use reth_db::database::Database;

use reth::tasks::pool::BlockingTaskPool;
use reth_payload_builder::database::CachedReads;
use serde::Deserialize;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

/// MergingBuilderConfig configures merging builder.
/// * `num_threads` - number of threads to use for merging.
/// * `merge_wait_time_ms` - time to wait for merging to finish before consuming new orders.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MergingBuilderConfig {
    pub discard_txs: bool,
    pub num_threads: usize,
    pub merge_wait_time_ms: u64,
    #[serde(default)]
    pub coinbase_payment: bool,
}

impl MergingBuilderConfig {
    fn merge_wait_time(&self) -> Duration {
        Duration::from_millis(self.merge_wait_time_ms)
    }
}

pub fn run_merging_builder<DB: Database + Clone + 'static>(
    input: LiveBuilderInput<DB>,
    config: &MergingBuilderConfig,
) {
    let block_number = input.ctx.block_env.number.to::<u64>();
    let mut order_intake_consumer =
        OrderIntakeStore::new(input.input, &input.sbundle_mergeabe_signers);

    let mut merging_combinator = CombinatorContext::new(
        input.provider_factory.clone(),
        input.root_hash_task_pool,
        input.ctx.clone(),
        vec![],
        input.cancel.clone(),
        None,
        config.discard_txs,
        config.coinbase_payment,
        input.root_hash_config,
        input.builder_name,
    );

    let mut merging_pool = MergingPool::new(
        input.provider_factory.clone(),
        input.ctx.clone(),
        config.num_threads,
        input.cancel.clone(),
    );

    let mut cached_groups = CachedGroups::new();

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

        let orders_closed_at = OffsetDateTime::now_utc();

        let new_orders = order_intake_consumer.drain_new_orders();
        // We can update cached_groups if we have ONLY adds
        if let Some(new_orders) = new_orders {
            cached_groups.add_orders(new_orders);
        } else {
            cached_groups = CachedGroups::new();
            cached_groups.add_orders(order_intake_consumer.get_orders());
        }

        let groups = cached_groups.get_order_groups();

        let group_count = groups.len();
        let order_count = groups.iter().map(|g| g.orders.len()).sum::<usize>();
        merging_combinator.set_groups(groups.clone());

        let start = Instant::now();
        if let Err(error) = merging_pool.stop_merging_threads() {
            error!(
                ?error,
                block_number, "Error stopping merging threads, cancelling slot, building block",
            );
            break 'building;
        }
        trace!(
            time_ms = start.elapsed().as_millis(),
            "Stopped merging threads"
        );
        if let Err(error) = merging_pool.start_merging_tasks(groups) {
            error!(
                ?error,
                block_number, "Error starting merging tasks, cancelling slot, building block",
            );
            break 'building;
        }

        let merging_start_time = Instant::now();
        trace!("Starting new merging batch");
        'merging: loop {
            if input.cancel.is_cancelled() {
                break 'building;
            }
            if merging_start_time.elapsed() > config.merge_wait_time() {
                break 'merging;
            }

            match merging_combinator.combine_best_groups_mergings(
                orders_closed_at,
                input.sink.can_use_suggested_fee_recipient_as_coinbase(),
            ) {
                Ok(block) => {
                    trace!(
                        group_count,
                        order_count,
                        bid_value = format_ether(block.built_block_trace().bid_value),
                        "Merger combined best group orderings"
                    );
                    input.sink.new_block(block);
                }
                Err(err) => {
                    if !handle_building_error(err) {
                        break 'building;
                    }
                }
            }
        }
    }
}

pub fn merging_build_backtest<DB: Database + Clone + 'static>(
    input: BacktestSimulateBlockInput<'_, DB>,
    config: MergingBuilderConfig,
) -> eyre::Result<(Block, CachedReads)> {
    let sorted_orders = {
        let mut orders = input.sim_orders.clone();
        orders.sort_by_key(|o| o.order.id());
        orders
    };

    let groups = split_orders_into_groups(sorted_orders);

    let mut merging_context = MergingContext::new(
        input.provider_factory.clone(),
        input.ctx.clone(),
        groups.clone(),
        CancellationToken::new(),
        input.cached_reads,
    );

    for (group_idx, group) in groups.iter().enumerate() {
        if group.orders.len() == 1 {
            continue;
        } else if group.orders.len() <= 3 {
            merging_context.run_merging_task(MergeTask {
                group_idx,
                command: MergeTaskCommand::AllPermutations,
            })?;
        } else {
            merging_context.run_merging_task(MergeTask {
                group_idx,
                command: MergeTaskCommand::StaticOrdering {
                    extra_orderings: true,
                },
            })?;
            merging_context.run_merging_task(MergeTask {
                group_idx,
                command: MergeTaskCommand::RandomPermutations { seed: 0, count: 10 },
            })?;
        }
    }

    let cache_reads = merging_context.into_cached_reads();

    let mut combinator_context = CombinatorContext::new(
        input.provider_factory,
        BlockingTaskPool::build()?,
        input.ctx.clone(),
        groups.clone(),
        CancellationToken::new(),
        Some(cache_reads),
        config.discard_txs,
        config.coinbase_payment,
        RootHashConfig::skip_root_hash(),
        input.builder_name,
    );

    let block_builder = combinator_context
        .combine_best_groups_mergings(OffsetDateTime::now_utc(), config.coinbase_payment)?;
    let payout_tx_value = if config.coinbase_payment {
        None
    } else {
        Some(block_builder.true_block_value()?)
    };
    let finalize_block_result = block_builder.finalize_block(payout_tx_value)?;
    Ok((
        finalize_block_result.block,
        finalize_block_result.cached_reads,
    ))
}

#[derive(Debug)]
pub struct MergingBuildingAlgorithm {
    root_hash_config: RootHashConfig,
    root_hash_task_pool: BlockingTaskPool,
    sbundle_mergeabe_signers: Vec<Address>,
    config: MergingBuilderConfig,
    name: String,
}

impl MergingBuildingAlgorithm {
    pub fn new(
        root_hash_config: RootHashConfig,
        root_hash_task_pool: BlockingTaskPool,
        sbundle_mergeabe_signers: Vec<Address>,
        config: MergingBuilderConfig,
        name: String,
    ) -> Self {
        Self {
            root_hash_config,
            root_hash_task_pool,
            sbundle_mergeabe_signers,
            config,
            name,
        }
    }
}

impl<DB: Database + Clone + 'static> BlockBuildingAlgorithm<DB> for MergingBuildingAlgorithm {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn build_blocks(&self, input: BlockBuildingAlgorithmInput<DB>) {
        let live_input = LiveBuilderInput {
            provider_factory: input.provider_factory,
            root_hash_config: self.root_hash_config.clone(),
            root_hash_task_pool: self.root_hash_task_pool.clone(),
            ctx: input.ctx.clone(),
            input: input.input,
            sink: input.sink,
            builder_name: self.name.clone(),
            cancel: input.cancel,
            sbundle_mergeabe_signers: self.sbundle_mergeabe_signers.clone(),
        };
        run_merging_builder(live_input, &self.config);
    }
}
