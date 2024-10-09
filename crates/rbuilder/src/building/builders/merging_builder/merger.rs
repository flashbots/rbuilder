use std::sync::Arc;

use crate::building::{BlockBuildingContext, BlockState, PartialBlock};
use ahash::HashMap;
use alloy_primitives::{Address, U256};
use itertools::Itertools;
use rand::{seq::SliceRandom, SeedableRng};
use reth::providers::{ProviderFactory, StateProvider};
use reth_db::database::Database;
use reth_payload_builder::database::CachedReads;

use tokio_util::sync::CancellationToken;

use super::OrderGroup;

/// MergeTask describes some ordering that should be tried for the given group.
#[derive(Debug, Clone)]
pub struct MergeTask {
    pub group_idx: usize,
    pub command: MergeTaskCommand,
}

#[derive(Debug, Clone)]
pub enum MergeTaskCommand {
    /// `StaticOrdering` checks the following ordrerings: map profit first / last, mev gas price first / last
    StaticOrdering {
        /// if false reverse gas price and reverse profit orderings are skipped
        extra_orderings: bool,
    },
    /// `AllPermutations` checks all possible permutations of the group.
    AllPermutations,
    /// `RandomPermutations` checks random permutations of the group.
    RandomPermutations { seed: u64, count: usize },
}

#[derive(Debug)]
pub struct MergingContext<DB> {
    pub provider_factory: ProviderFactory<DB>,
    pub ctx: BlockBuildingContext,
    pub groups: Vec<OrderGroup>,
    pub cancellation_token: CancellationToken,
    pub cache: Option<CachedReads>,
}

impl<DB: Database + Clone> MergingContext<DB> {
    pub fn new(
        provider_factory: ProviderFactory<DB>,
        ctx: BlockBuildingContext,
        groups: Vec<OrderGroup>,
        cancellation_token: CancellationToken,
        cache: Option<CachedReads>,
    ) -> Self {
        MergingContext {
            provider_factory,
            ctx,
            groups,
            cancellation_token,
            cache,
        }
    }

    pub fn run_merging_task(&mut self, task: MergeTask) -> eyre::Result<()> {
        let state_provider = self
            .provider_factory
            .history_by_block_hash(self.ctx.attributes.parent)?;
        let state_provider: Arc<dyn StateProvider> = Arc::from(state_provider);

        let orderings_to_try = match task.command {
            MergeTaskCommand::StaticOrdering { extra_orderings } => {
                // order by / reverse gas price
                // order by / reverse max profit

                let mut orderings = vec![];
                let orders = &self.groups[task.group_idx];
                {
                    let mut ids_and_value = orders
                        .orders
                        .iter()
                        .enumerate()
                        .map(|(idx, o)| (idx, o.sim_value.mev_gas_price))
                        .collect::<Vec<_>>();

                    if extra_orderings {
                        ids_and_value.sort_by(|a, b| a.1.cmp(&b.1));
                        orderings.push(
                            ids_and_value
                                .iter()
                                .map(|(idx, _)| *idx)
                                .collect::<Vec<_>>(),
                        );
                    }
                    ids_and_value.sort_by(|a, b| a.1.cmp(&b.1).reverse());
                    orderings.push(
                        ids_and_value
                            .iter()
                            .map(|(idx, _)| *idx)
                            .collect::<Vec<_>>(),
                    );

                    let mut ids_and_value = orders
                        .orders
                        .iter()
                        .enumerate()
                        .map(|(idx, o)| (idx, o.sim_value.coinbase_profit))
                        .collect::<Vec<_>>();

                    if extra_orderings {
                        ids_and_value.sort_by(|a, b| a.1.cmp(&b.1));
                        orderings.push(
                            ids_and_value
                                .iter()
                                .map(|(idx, _)| *idx)
                                .collect::<Vec<_>>(),
                        );
                    }

                    ids_and_value.sort_by(|a, b| a.1.cmp(&b.1).reverse());
                    orderings.push(
                        ids_and_value
                            .iter()
                            .map(|(idx, _)| *idx)
                            .collect::<Vec<_>>(),
                    );
                }
                orderings
            }
            MergeTaskCommand::AllPermutations => {
                let orders = &self.groups[task.group_idx];
                let orderings = (0..orders.orders.len()).collect::<Vec<_>>();
                orderings
                    .into_iter()
                    .permutations(orders.orders.len())
                    .collect()
            }
            MergeTaskCommand::RandomPermutations { seed, count } => {
                let mut orderings = vec![];

                let orders = &self.groups[task.group_idx];
                let mut indexes = (0..orders.orders.len()).collect::<Vec<_>>();
                let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
                for _ in 0..count {
                    indexes.shuffle(&mut rng);
                    orderings.push(indexes.clone());
                }

                orderings
            }
        };

        for ordering in orderings_to_try {
            let mut partial_block = PartialBlock::new(true, None);
            let mut state = BlockState::new_arc(state_provider.clone())
                .with_cached_reads(self.cache.take().unwrap_or_default());
            partial_block.pre_block_call(&self.ctx, &mut state)?;

            let mut ordering = ordering;
            ordering.reverse(); // we will use it as a queue: pop() and push()
            let mut result_ordering = Vec::new();
            let mut total_profit = U256::ZERO;
            let mut pending_orders: HashMap<(Address, u64), usize> = HashMap::default();
            loop {
                if self.cancellation_token.is_cancelled() {
                    let (cached_reads, _, _) = state.into_parts();
                    self.cache = Some(cached_reads);
                    return Ok(());
                }

                let order_idx = if let Some(order_idx) = ordering.pop() {
                    order_idx
                } else {
                    break;
                };
                let sim_order = &self.groups[task.group_idx].orders[order_idx];
                let commit_result = partial_block.commit_order(sim_order, &self.ctx, &mut state)?;
                match commit_result {
                    Ok(res) => {
                        for (address, nonce) in res.nonces_updated {
                            if let Some(pending_order) = pending_orders.remove(&(address, nonce)) {
                                ordering.push(pending_order);
                            }
                        }
                        total_profit += res.coinbase_profit;
                        result_ordering.push((order_idx, res.coinbase_profit));
                    }
                    Err(err) => {
                        if let Some((address, nonce)) =
                            err.try_get_tx_too_high_error(&sim_order.order)
                        {
                            pending_orders.insert((address, nonce), order_idx);
                        }
                    }
                }
            }

            let mut best_ordering = self.groups[task.group_idx].best_ordering.lock().unwrap();
            if best_ordering.total_profit < total_profit {
                best_ordering.total_profit = total_profit;
                best_ordering.orders = result_ordering;
            }

            let (new_cached_reads, _, _) = state.into_parts();
            self.cache = Some(new_cached_reads);
        }

        Ok(())
    }

    pub fn into_cached_reads(self) -> CachedReads {
        self.cache.unwrap_or_default()
    }
}
