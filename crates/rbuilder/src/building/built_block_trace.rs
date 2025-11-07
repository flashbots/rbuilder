use crate::building::builders::BuiltBlockId;

use super::ExecutionResult;
use ahash::{AHasher, HashMap, HashSet};
use alloy_primitives::{Address, TxHash, I256, U256};
use rbuilder_primitives::{
    order_statistics::OrderStatistics, Order, OrderId, OrderReplacementKey, SimulatedOrder,
};
use std::{collections::hash_map, hash::Hasher, time::Duration};
use time::OffsetDateTime;

/// Structs for recording data about a built block, such as what bundles were included, and where txs came from.
/// Trace can be used to verify bundle invariants.

#[derive(Debug, Clone)]
pub struct BuiltBlockTrace {
    pub build_block_id: BuiltBlockId,
    pub included_orders: Vec<ExecutionResult>,
    /// How much we bid (pay to the validator)
    pub bid_value: U256,
    /// Subsidy used in the bid.
    pub subsidy: I256,
    /// coinbase balance delta before the payout tx.
    pub coinbase_reward: U256,
    /// True block value (coinbase balance delta) excluding the cost of the payout to validator
    pub true_bid_value: U256,
    /// Amount that is left out on the coinbase to pay for mev blocker orderflow
    pub mev_blocker_price: U256,
    /// Timestamp of the moment we stopped considering new orders for this block.
    pub orders_closed_at: OffsetDateTime,
    /// UnfinishedBuiltBlocksInput chose this block as the best block and sent it downstream
    pub chosen_as_best_at: OffsetDateTime,
    /// Block was sent to the bidder (SlotBidder::notify_new_built_block)
    pub sent_to_bidder: OffsetDateTime,
    /// Bid received from the bidder (UnfinishedBuiltBlocksInput::seal_command)
    pub bid_received_at: OffsetDateTime,
    /// Bid sent to the sealer thread
    pub sent_to_sealer: OffsetDateTime,
    /// Sealer picked by sealer thread
    pub picked_by_sealer_at: OffsetDateTime,
    /// Timestamp when this block was fully sealed and ready for submission.
    pub orders_sealed_at: OffsetDateTime,

    pub fill_time: Duration,
    pub finalize_time: Duration,
    pub finalize_adjust_time: Duration,
    /// Overhead added by creating the MultiPrefinalizedBlock which makes some extra copies.
    pub multi_bid_copy_duration: Duration,
    pub root_hash_time: Duration,
    /// Value we saw in the competition when we decided to make this bid.
    pub seen_competition_bid: Option<U256>,
    /// Orders we had available to build the block (we might have not use all of them because of timeouts)
    pub available_orders_statistics: OrderStatistics,
    /// Every call to BlockBuildingHelper::commit_order impacts here.
    pub considered_orders_statistics: OrderStatistics,
    /// Anything we call BlockBuildingHelper::commit_order on but didn't include (redundant with considered_orders_statistics-included_orders)
    pub failed_orders_statistics: OrderStatistics,

    /// Every call to BlockBuildingHelper::commit_order during pre-filtered build step impacts here.
    pub filtered_build_considered_orders_statistics: OrderStatistics,
    /// Anything we call BlockBuildingHelper::commit_order on but didn't include (redundant with filtered_build_considered_orders_statistics-included_orders) during pre-filtered build step
    pub filtered_build_failed_orders_statistics: OrderStatistics,
}

#[derive(thiserror::Error, Debug)]
pub enum BuiltBlockTraceError {
    #[error("More than one order is included with the same replacement data: {0:?}")]
    DuplicateReplacementData(OrderReplacementKey),
    #[error("Included order had tx from or to blocked address")]
    BlockedAddress,
    #[error(
        "Bundle tx reverted that is not revertable, order: {order_id:?}, tx_hash: {tx_hash:?}"
    )]
    BundleTxReverted { order_id: OrderId, tx_hash: TxHash },
}

impl BuiltBlockTrace {
    pub fn new(build_block_id: BuiltBlockId) -> Self {
        Self {
            included_orders: Vec::new(),
            bid_value: U256::from(0),
            coinbase_reward: U256::from(0),
            true_bid_value: U256::from(0),
            mev_blocker_price: U256::from(0),
            orders_closed_at: OffsetDateTime::now_utc(),
            orders_sealed_at: OffsetDateTime::now_utc(),
            fill_time: Duration::from_secs(0),
            finalize_time: Duration::from_secs(0),
            finalize_adjust_time: Duration::from_secs(0),
            root_hash_time: Duration::from_secs(0),
            seen_competition_bid: None,
            considered_orders_statistics: Default::default(),
            failed_orders_statistics: Default::default(),
            available_orders_statistics: Default::default(),
            filtered_build_considered_orders_statistics: Default::default(),
            filtered_build_failed_orders_statistics: Default::default(),
            chosen_as_best_at: OffsetDateTime::now_utc(),
            sent_to_bidder: OffsetDateTime::now_utc(),
            bid_received_at: OffsetDateTime::now_utc(),
            sent_to_sealer: OffsetDateTime::now_utc(),
            picked_by_sealer_at: OffsetDateTime::now_utc(),
            build_block_id,
            subsidy: I256::ZERO,
            multi_bid_copy_duration: Duration::ZERO,
        }
    }

    pub fn set_filtered_build_statistics(
        &mut self,
        considered_orders_statistics: OrderStatistics,
        failed_orders_statistics: OrderStatistics,
    ) {
        self.filtered_build_considered_orders_statistics = considered_orders_statistics;
        self.filtered_build_failed_orders_statistics = failed_orders_statistics;
    }

    /// Should be called after block is sealed
    /// Sets:
    /// orders_sealed_at to the current time
    pub fn update_orders_sealed_at(&mut self) {
        self.orders_sealed_at = OffsetDateTime::now_utc();
    }

    /// Call after a commit_order ok
    pub fn add_included_order(&mut self, execution_result: ExecutionResult) {
        self.included_orders.push(execution_result);
    }

    /// Call before commit_order
    pub fn add_considered_order(&mut self, sim_order: &SimulatedOrder) {
        self.considered_orders_statistics.add(&sim_order.order);
    }

    /// Call after a commit_order Err
    pub fn add_failed_order(&mut self, sim_order: &SimulatedOrder) {
        self.failed_orders_statistics.add(&sim_order.order);
    }

    // txs, bundles, share bundles
    pub fn used_order_count(&self) -> (usize, usize, usize) {
        self.included_orders
            .iter()
            .fold((0, 0, 0), |acc, order| match order.order {
                Order::Tx(_) => (acc.0 + 1, acc.1, acc.2),
                Order::Bundle(_) => (acc.0, acc.1 + 1, acc.2),
                Order::ShareBundle(_) => (acc.0, acc.1, acc.2 + 1),
            })
    }

    pub fn verify_bundle_consistency(
        &self,
        blocklist: &HashSet<Address>,
    ) -> Result<(), BuiltBlockTraceError> {
        let mut bundle_txs_scratchpad = HashMap::default();
        let mut executed_tx_hashes_scratchpad = Vec::new();

        for res in &self.included_orders {
            let executed_tx_hashes = {
                executed_tx_hashes_scratchpad.clear();
                &mut executed_tx_hashes_scratchpad
            };
            for tx_info in &res.tx_infos {
                executed_tx_hashes.push((tx_info.tx.hash(), tx_info.receipt.success));
                if blocklist.contains(&tx_info.tx.signer())
                    || tx_info
                        .tx
                        .to()
                        .map(|to| blocklist.contains(&to))
                        .unwrap_or(false)
                {
                    return Err(BuiltBlockTraceError::BlockedAddress);
                }
            }

            let bundle_txs = {
                // we can have the same tx in the list_txs() multiple times(share bundle merging)
                // sometimes that tx is marked as revertible and sometimes not
                // if tx is marked as revertible in one sub-bundle but not another we consider that tx as revertible
                bundle_txs_scratchpad.clear();
                for (tx, can_revert) in res.order.list_txs() {
                    let hash = tx.hash();
                    match bundle_txs_scratchpad.entry(hash) {
                        hash_map::Entry::Vacant(entry) => {
                            entry.insert(can_revert);
                        }
                        hash_map::Entry::Occupied(mut entry) => {
                            let can_revert_stored = entry.get();
                            if !can_revert_stored && can_revert {
                                entry.insert(can_revert);
                            }
                        }
                    }
                }
                &bundle_txs_scratchpad
            };
            for (executed_hash, success) in executed_tx_hashes {
                if let Some(can_revert) = bundle_txs.get(executed_hash) {
                    if !*success && !can_revert {
                        return Err(BuiltBlockTraceError::BundleTxReverted {
                            order_id: res.order.id(),
                            tx_hash: *executed_hash,
                        });
                    }
                }
            }
        }

        Ok(())
    }

    /// Generates a cheap hash to identify the tx content.
    pub fn transactions_hash(&self) -> u64 {
        let mut hasher = AHasher::default();
        for execution_result in &self.included_orders {
            for tx in execution_result.tx_infos.iter().map(|info| &info.tx) {
                let tx_hash = tx.hash();
                hasher.write(tx_hash.as_slice());
            }
        }
        hasher.finish()
    }
}
