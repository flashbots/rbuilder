mod backtest_build_block;
mod backtest_build_range;
pub mod execute;
pub mod fetch;

mod results_store;
mod store;
pub use backtest_build_block::run_backtest_build_block;
pub use backtest_build_range::run_backtest_build_range;
use std::collections::HashSet;

use crate::{
    mev_boost::BuilderBlockReceived,
    primitives::{
        serialize::{RawOrder, RawOrderConvertError, TxEncoding},
        AccountNonce, Order, SimValue,
    },
};
use alloy_primitives::TxHash;
use alloy_rpc_types::{BlockTransactions, Transaction};
use serde::{Deserialize, Serialize};

pub use fetch::HistoricalDataFetcher;
pub use results_store::{BacktestResultsStorage, StoredBacktestResult};
pub use store::HistoricalDataStorage;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RawOrdersWithTimestamp {
    pub timestamp_ms: u64,
    pub order: RawOrder,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sim_value: Option<SimValue>,
}

impl From<OrdersWithTimestamp> for RawOrdersWithTimestamp {
    fn from(orders: OrdersWithTimestamp) -> Self {
        Self {
            timestamp_ms: orders.timestamp_ms,
            order: orders.order.into(),
            sim_value: orders.sim_value,
        }
    }
}

impl RawOrdersWithTimestamp {
    fn decode(self, encoding: TxEncoding) -> Result<OrdersWithTimestamp, RawOrderConvertError> {
        Ok(OrdersWithTimestamp {
            timestamp_ms: self.timestamp_ms,
            order: self.order.decode(encoding)?,
            sim_value: self.sim_value,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OrdersWithTimestamp {
    pub timestamp_ms: u64,
    pub order: Order,
    pub sim_value: Option<SimValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockData {
    pub block_number: u64,
    pub winning_bid_trace: BuilderBlockReceived,
    // pub onchain_block: Block,
    pub onchain_block: alloy_rpc_types::Block,
    pub available_orders: Vec<OrdersWithTimestamp>,
}

impl BlockData {
    pub fn filter_orders_by_block_lag(&mut self, build_block_lag_ms: i64) {
        self.available_orders.retain(|orders| {
            orders.timestamp_ms as i64
                <= self.winning_bid_trace.timestamp_ms as i64 - build_block_lag_ms
        });
    }

    pub fn filter_orders_by_ids(&mut self, order_ids: &[String]) {
        self.available_orders
            .retain(|order| order_ids.contains(&order.order.id().to_string()));
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
                if !available_txs.contains(&tx.hash) && !self.is_validator_fee_payment(tx) {
                    result.push(tx.hash);
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
                    !available_accounts
                        .iter()
                        .any(|x| x.nonce == tx.nonce && x.address == tx.from)
                })
                .map(|tx| {
                    (
                        tx.hash,
                        AccountNonce {
                            nonce: tx.nonce,
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
        tx.from == self.onchain_block.header.miner
            && tx
                .to
                .is_some_and(|to| to == self.winning_bid_trace.proposer_fee_recipient)
    }
}
