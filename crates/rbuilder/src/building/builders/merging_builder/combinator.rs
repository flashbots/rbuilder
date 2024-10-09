use crate::{
    building::{
        builders::block_building_helper::{BlockBuildingHelper, BlockBuildingHelperFromDB},
        BlockBuildingContext,
    },
    roothash::RootHashConfig,
};

use crate::utils::check_provider_factory_health;
use reth::{providers::ProviderFactory, tasks::pool::BlockingTaskPool};
use reth_db::database::Database;
use reth_payload_builder::database::CachedReads;
use std::time::Instant;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{info_span, trace};

use super::{GroupOrdering, OrderGroup};

/// CombinatorContext is used for merging the best ordering from groups into final block.
#[derive(Debug)]
pub struct CombinatorContext<DB> {
    provider_factory: ProviderFactory<DB>,
    root_hash_task_pool: BlockingTaskPool,
    ctx: BlockBuildingContext,
    groups: Vec<OrderGroup>,
    cancellation_token: CancellationToken,
    cached_reads: Option<CachedReads>,
    discard_txs: bool,
    coinbase_payment: bool,
    root_hash_config: RootHashConfig,
    builder_name: String,
}

impl<DB: Database + Clone + 'static> CombinatorContext<DB> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider_factory: ProviderFactory<DB>,
        root_hash_task_pool: BlockingTaskPool,
        ctx: BlockBuildingContext,
        groups: Vec<OrderGroup>,
        cancellation_token: CancellationToken,
        cached_reads: Option<CachedReads>,
        discard_txs: bool,
        coinbase_payment: bool,
        root_hash_config: RootHashConfig,
        builder_name: String,
    ) -> Self {
        CombinatorContext {
            provider_factory,
            root_hash_task_pool,
            ctx,
            groups,
            cancellation_token,
            cached_reads,
            discard_txs,
            coinbase_payment,
            root_hash_config,
            builder_name,
        }
    }

    pub fn set_groups(&mut self, groups: Vec<OrderGroup>) {
        self.groups = groups;
    }

    /// Checks for simulated bundles that generated kickbacks.
    /// orderings MUST be the same size as self.groups
    fn contains_kickbacks(&self, orderings: &[GroupOrdering]) -> bool {
        orderings.iter().enumerate().any(|(group_idx, ordering)| {
            ordering.orders.iter().any(|(order_idx, _)| {
                !self.groups[group_idx].orders[*order_idx]
                    .sim_value
                    .paid_kickbacks
                    .is_empty()
            })
        })
    }

    pub fn combine_best_groups_mergings(
        &mut self,
        orders_closed_at: OffsetDateTime,
        can_use_suggested_fee_recipient_as_coinbase: bool,
    ) -> eyre::Result<Box<dyn BlockBuildingHelper>> {
        let build_attempt_id: u32 = rand::random();
        let span = info_span!("build_run", build_attempt_id);
        let _guard = span.enter();
        check_provider_factory_health(self.ctx.block(), &self.provider_factory)?;

        let build_start = Instant::now();

        let mut best_orderings: Vec<GroupOrdering> = self
            .groups
            .iter()
            .map(|g| g.best_ordering.lock().unwrap().clone())
            .collect();

        let use_suggested_fee_recipient_as_coinbase = self.coinbase_payment
            && !self.contains_kickbacks(&best_orderings)
            && can_use_suggested_fee_recipient_as_coinbase;
        // Create a new ctx to remove builder_signer if necessary
        let mut ctx = self.ctx.clone();
        if use_suggested_fee_recipient_as_coinbase {
            ctx.modify_use_suggested_fee_recipient_as_coinbase();
        }

        let mut block_building_helper = BlockBuildingHelperFromDB::new(
            self.provider_factory.clone(),
            self.root_hash_task_pool.clone(),
            self.root_hash_config.clone(),
            ctx,
            // @Perf cached reads / cursor caches
            None,
            self.builder_name.clone(),
            self.discard_txs,
            None,
            self.cancellation_token.clone(),
        )?;
        block_building_helper.set_trace_orders_closed_at(orders_closed_at);
        // loop until we insert all orders into the block
        loop {
            if self.cancellation_token.is_cancelled() {
                break;
            }

            let sim_order = if let Some((group_idx, order_idx, _)) = best_orderings
                .iter()
                .enumerate()
                .filter_map(|(group_idx, ordering)| {
                    ordering
                        .orders
                        .first()
                        .map(|(order_idx, order_profit)| (group_idx, *order_idx, *order_profit))
                })
                .max_by_key(|(_, _, p)| *p)
            {
                best_orderings[group_idx].orders.remove(0);
                &self.groups[group_idx].orders[order_idx]
            } else {
                // no order left in the groups
                break;
            };

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
        }
        block_building_helper.set_trace_fill_time(build_start.elapsed());
        self.cached_reads = Some(block_building_helper.clone_cached_reads());
        Ok(Box::new(block_building_helper))
    }
}
