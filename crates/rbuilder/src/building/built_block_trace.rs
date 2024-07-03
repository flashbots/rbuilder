use super::{BundleErr, ExecutionError, ExecutionResult, OrderErr};
use crate::primitives::{Order, OrderReplacementKey};
use ahash::{HashMap, HashSet};
use alloy_primitives::{Address, U256};
use std::time::Duration;
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
    #[error("Bundle tx reverted that is not revertable")]
    BundleTxReverted,
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
        }
    }

    /// Should be called after block is sealed
    /// Sets:
    /// orders_sealed_at to the current time
    /// orders_closed_at to the given time
    pub fn update_orders_timestamps_after_block_sealed(
        &mut self,
        orders_closed_at: OffsetDateTime,
    ) {
        self.orders_closed_at = orders_closed_at;
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

    pub fn verify_bundle_consistency(&self, blocklist: &HashSet<Address>) -> eyre::Result<(), BuiltBlockTraceError> {
        let mut replacement_data_count: HashSet<_> = HashSet::default();

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

            let mut executed_tx_hashes = Vec::with_capacity(res.txs.len());
            for (tx, receipt) in res.txs.iter().zip(res.receipts.iter()) {
                let tx = &tx.tx;
                executed_tx_hashes.push((tx.hash(), receipt.success));
                if blocklist.contains(&tx.signer())
                    || tx.to().map(|to| blocklist.contains(&to)).unwrap_or(false)
                {
                    return Err(BuiltBlockTraceError::BlockedAddress);
                }
            }

            let bundle_txs = res
                .order
                .list_txs()
                .into_iter()
                .map(|(tx, can_revert)| (tx.hash(), can_revert))
                .collect::<HashMap<_, _>>();
            for (executed_hash, success) in executed_tx_hashes {
                if let Some(can_revert) = bundle_txs.get(&executed_hash) {
                    if !success && !can_revert {
                        return Err(BuiltBlockTraceError::BundleTxReverted);
                    }
                }
            }
        }

        Ok(())
    }
}