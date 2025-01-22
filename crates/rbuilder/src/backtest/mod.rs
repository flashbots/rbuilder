mod backtest_build_range;
pub mod execute;
pub mod fetch;

pub mod build_block;
pub mod redistribute;
pub mod restore_landed_orders;
mod results_store;
mod store;

pub use backtest_build_range::run_backtest_build_range;
use std::collections::HashSet;

use crate::{
    mev_boost::BuilderBlockReceived,
    primitives::{
        serialize::{RawOrder, RawOrderConvertError, TxEncoding},
        AccountNonce, Order, OrderId, OrderReplacementKey,
    },
    utils::offset_datetime_to_timestamp_ms,
};
use alloy_consensus::Transaction as TransactionTrait;
use alloy_network_primitives::TransactionResponse;
use alloy_primitives::{Address, TxHash, I256};
use alloy_rpc_types::{BlockTransactions, Transaction};
pub use fetch::HistoricalDataFetcher;
pub use results_store::{BacktestResultsStorage, StoredBacktestResult};
use serde::{Deserialize, Serialize};
pub use store::HistoricalDataStorage;
use time::OffsetDateTime;
use tracing::trace;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RawOrdersWithTimestamp {
    pub timestamp_ms: u64,
    pub order: RawOrder,
}

impl From<OrdersWithTimestamp> for RawOrdersWithTimestamp {
    fn from(orders: OrdersWithTimestamp) -> Self {
        Self {
            timestamp_ms: orders.timestamp_ms,
            order: orders.order.into(),
        }
    }
}

impl RawOrdersWithTimestamp {
    fn decode(self, encoding: TxEncoding) -> Result<OrdersWithTimestamp, RawOrderConvertError> {
        Ok(OrdersWithTimestamp {
            timestamp_ms: self.timestamp_ms,
            order: self.order.decode(encoding)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OrdersWithTimestamp {
    pub timestamp_ms: u64,
    pub order: Order,
}

/// Historic data for a block.
/// Used for backtesting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltBlockData {
    pub included_orders: Vec<OrderId>,
    pub orders_closed_at: OffsetDateTime,
    pub sealed_at: OffsetDateTime,
    pub profit: I256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockData {
    pub block_number: u64,
    /// Extra info for landed block (not contained on onchain_block).
    /// We get this from the relays (API /relay/v1/data/bidtraces/builder_blocks_received).
    pub winning_bid_trace: BuilderBlockReceived,
    /// Landed block.
    pub onchain_block: alloy_rpc_types::Block,
    /// Orders we had at the moment of building the block.
    /// This might be an approximation depending on DataSources used.
    pub available_orders: Vec<OrdersWithTimestamp>,
    pub filtered_orders: HashSet<OrderId>,
    pub built_block_data: Option<BuiltBlockData>,
}

impl BlockData {
    /// Filters orders that arrived after we started building the block.
    pub fn filter_late_orders(&mut self, build_block_lag_ms: i64) {
        let final_timestamp_ms = self.winning_bid_trace.timestamp_ms as i64 - build_block_lag_ms;
        self.filter_orders_by_end_timestamp_ms(final_timestamp_ms as u64);
    }

    pub fn filter_orders_by_end_timestamp(&mut self, final_timestamp: OffsetDateTime) {
        let final_timestamp_ms = offset_datetime_to_timestamp_ms(final_timestamp);
        self.filter_orders_by_end_timestamp_ms(final_timestamp_ms);
    }

    fn filter_orders_by_end_timestamp_ms(&mut self, final_timestamp_ms: u64) {
        self.available_orders.retain(|orders| {
            if orders.timestamp_ms <= final_timestamp_ms {
                true
            } else {
                trace!(order = ?orders.order.id(), "order filtered by end timestamp");
                self.filtered_orders.insert(orders.order.id());
                false
            }
        });

        // make sure that we have only one copy of the cancellable orders
        // we use timestamp and not replacement sequence number because of the limitation of the backtest

        // sort orders by timestamp from latest to earliest (high timestamp to low)
        self.available_orders
            .sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));
        let mut replacement_keys_seen: HashSet<OrderReplacementKey> = HashSet::default();

        self.available_orders.retain(|orders| {
            if let Some(key) = orders.order.replacement_key() {
                if replacement_keys_seen.contains(&key) {
                    trace!(order = ?orders.order.id(), "order filtered by end timestamp");
                    self.filtered_orders.insert(orders.order.id());
                    return false;
                }
                replacement_keys_seen.insert(key);
            }
            true
        });
    }

    /// This will remove all bundles that have all transactions available in the public mempool
    pub fn filter_bundles_from_mempool(&mut self) {
        let mempool_txs = self
            .available_orders
            .iter()
            .filter_map(|o| match &o.order {
                Order::Tx(tx) => Some(tx.tx_with_blobs.hash()),
                _ => None,
            })
            .collect::<HashSet<_>>();

        self.available_orders.retain(|orders| {
            if orders.order.is_tx() {
                return true;
            };
            let txs = orders.order.list_txs();
            if txs.iter().any(|(tx, _)| !mempool_txs.contains(&tx.hash())) {
                true
            } else {
                trace!(order = ?orders.order.id(), "order filtered from public mempool");
                self.filtered_orders.insert(orders.order.id());
                false
            }
        });
    }

    pub fn filter_orders_by_ids(&mut self, order_ids: &[String]) {
        self.available_orders.retain(|order| {
            if order_ids.contains(&order.order.id().to_string()) {
                true
            } else {
                trace!(order = ?order.order.id(), "order filtered by id");
                self.filtered_orders.insert(order.order.id());
                false
            }
        });
    }

    pub fn filter_out_ignored_signers(&mut self, ignored_signers: &[Address]) {
        self.available_orders.retain(|orders| {
            let order = &orders.order;
            let signer = if let Some(signer) = order.signer() {
                signer
            } else {
                return true;
            };
            if !ignored_signers.contains(&signer) {
                true
            } else {
                trace!(order = ?order.id(), "order filtered by ignored signers");
                self.filtered_orders.insert(order.id());
                false
            }
        });
    }

    /// Returns tx's hashes on onchain_block not found on any available_orders, except for the validator payment tx.
    pub fn search_missing_txs_on_available_orders(&self) -> Vec<TxHash> {
        let mut result = Vec::new();
        let mut available_txs = HashSet::new();
        for order in self.available_orders.iter().map(|owt| &owt.order) {
            available_txs.extend(order.list_txs().iter().map(|(tx, _)| tx.hash()));
        }
        if let BlockTransactions::Full(txs) = &self.onchain_block.transactions {
            for tx in txs {
                if !available_txs.contains(&tx.tx_hash()) && !self.is_validator_fee_payment(tx) {
                    result.push(tx.tx_hash());
                }
            }
        } else {
            panic!("BlockTransactions::Full not found. This should not happen since we store full txs on our HistoricalDataStorage");
        }
        result
    }

    /// Returns landed txs targeting account nonces non of our available txs were targeting.
    pub fn search_missing_account_nonce_on_available_orders(&self) -> Vec<(TxHash, AccountNonce)> {
        let mut available_accounts = HashSet::new();
        for order in self.available_orders.iter().map(|owt| &owt.order) {
            let nonces = order.nonces();
            available_accounts.extend(nonces);
        }

        if let BlockTransactions::Full(txs) = &self.onchain_block.transactions {
            txs.iter()
                .filter(|tx| {
                    let tx = *tx;
                    !available_accounts
                        .iter()
                        .any(|x| x.nonce == tx.nonce() && x.address == tx.from)
                })
                .map(|tx| {
                    (
                        tx.tx_hash(),
                        AccountNonce {
                            nonce: tx.nonce(),
                            account: tx.from,
                        },
                    )
                })
                .collect()
        } else {
            panic!("BlockTransactions::Full not found. This should not happen since we store full txs on our HistoricalDataStorage");
        }
    }

    fn is_validator_fee_payment(&self, tx: &Transaction) -> bool {
        tx.from == self.onchain_block.header.beneficiary
            && TransactionTrait::to(tx)
                .is_some_and(|to| to == self.winning_bid_trace.proposer_fee_recipient)
    }
}
