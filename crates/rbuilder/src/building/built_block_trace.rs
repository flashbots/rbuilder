use super::{BundleErr, ExecutionError, ExecutionResult, OrderErr};
use crate::primitives::{Order, OrderId, OrderReplacementKey};
use ahash::{HashMap, HashSet};
use alloy_primitives::{Address, TxHash, U256};
use std::{collections::hash_map, time::Duration};
use time::OffsetDateTime;

/// Structs for recording data about a built block, such as what bundles were included, and where txs came from.
/// Trace can be used to verify bundle invariants.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltBlockTrace {
    pub included_orders: Vec<ExecutionResult>,
    /// How much we bid (pay to the validator)
    pub bid_value: U256,
    /// True block value (coinbase balance delta) excluding the cost of the payout to validator
    pub true_bid_value: U256,
    /// Some bundle failed with BundleErr::NoSigner, we might want to switch to !use_suggested_fee_recipient_as_coinbase
    pub got_no_signer_error: bool,
    pub orders_closed_at: OffsetDateTime,
    pub orders_sealed_at: OffsetDateTime,
    pub fill_time: Duration,
    pub finalize_time: Duration,
    pub root_hash_time: Duration,
}

impl Default for BuiltBlockTrace {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(thiserror::Error, Debug)]
pub enum BuiltBlockTraceError {
    #[error("More than one order is included with the same replacement data: {0:?}")]
    DuplicateReplacementData(OrderReplacementKey),
    #[error("Included order had different number of txs and receipts")]
    DifferentTxsAndReceipts,
    #[error("Included order had tx from or to blocked address")]
    BlockedAddress,
    #[error(
        "Bundle tx reverted that is not revertable, order: {order_id:?}, tx_hash: {tx_hash:?}"
    )]
    BundleTxReverted { order_id: OrderId, tx_hash: TxHash },
}

impl BuiltBlockTrace {
    pub fn new() -> Self {
        Self {
            included_orders: Vec::new(),
            bid_value: U256::from(0),
            true_bid_value: U256::from(0),
            got_no_signer_error: false,
            orders_closed_at: OffsetDateTime::now_utc(),
            orders_sealed_at: OffsetDateTime::now_utc(),
            fill_time: Duration::from_secs(0),
            finalize_time: Duration::from_secs(0),
            root_hash_time: Duration::from_secs(0),
        }
    }

    /// Should be called after block is sealed
    /// Sets:
    /// orders_sealed_at to the current time
    /// orders_closed_at to the given time
    pub fn update_orders_sealed_at(&mut self) {
        self.orders_sealed_at = OffsetDateTime::now_utc();
    }

    /// Call after a commit_order ok
    pub fn add_included_order(&mut self, execution_result: ExecutionResult) {
        self.included_orders.push(execution_result);
    }

    /// Call after a commit_order error
    pub fn modify_payment_when_no_signer_error(&mut self, err: &ExecutionError) {
        if let ExecutionError::OrderError(OrderErr::Bundle(BundleErr::NoSigner)) = err {
            self.got_no_signer_error = true
        }
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
        let mut replacement_data_count: HashSet<_> = HashSet::default();
        let mut bundle_txs_scratchpad = HashMap::default();
        let mut executed_tx_hashes_scratchpad = Vec::new();

        for res in &self.included_orders {
            for order in res.order.original_orders() {
                if let Some(data) = order.replacement_key() {
                    if replacement_data_count.contains(&data) {
                        return Err(BuiltBlockTraceError::DuplicateReplacementData(data));
                    }
                    replacement_data_count.insert(data);
                }
            }

            if res.txs.len() != res.receipts.len() {
                return Err(BuiltBlockTraceError::DifferentTxsAndReceipts);
            }

            let executed_tx_hashes = {
                executed_tx_hashes_scratchpad.clear();
                &mut executed_tx_hashes_scratchpad
            };
            for (tx, receipt) in res.txs.iter().zip(res.receipts.iter()) {
                executed_tx_hashes.push((tx.hash(), receipt.success));
                if blocklist.contains(&tx.signer())
                    || tx.to().map(|to| blocklist.contains(&to)).unwrap_or(false)
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
}
