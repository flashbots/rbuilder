pub mod flashbots_db;
pub mod mempool;
pub mod mev_boost;

use crate::{backtest::BlockData, mev_boost::BuilderBlockReceived, utils::timestamp_as_u64};

use alloy_provider::Provider;
use alloy_rpc_types::{Block, BlockId, BlockNumberOrTag};

use eyre::WrapErr;
use flashbots_db::RelayDB;
use futures::TryStreamExt;
use sqlx::PgPool;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use time::{Duration, OffsetDateTime};
use tokio::sync::Mutex;
use tracing::{info, trace};

use crate::{
    backtest::{fetch::mev_boost::PayloadDeliveredFetcher, OrdersWithTimestamp},
    utils::BoxedProvider,
};

#[derive(Debug, Clone)]
pub struct HistoricalDataFetcher {
    eth_provider: BoxedProvider,
    eth_rpc_parallel: usize,
    mempool_datadir: PathBuf,
    // If none, skip bundles
    flashbots_db: Option<PgPool>,
    payload_delivered_fetcher: PayloadDeliveredFetcher,
}

impl HistoricalDataFetcher {
    pub fn new(
        eth_provider: BoxedProvider,
        eth_rpc_parallel: usize,
        mempool_datadir: PathBuf,
        flashbots_db: Option<PgPool>,
    ) -> Self {
        Self {
            eth_provider,
            eth_rpc_parallel,
            mempool_datadir,
            flashbots_db,
            payload_delivered_fetcher: PayloadDeliveredFetcher::default(),
        }
    }

    async fn get_payload_delivered_bid_trace(
        &self,
        block_number: u64,
    ) -> eyre::Result<BuilderBlockReceived> {
        let res = self
            .payload_delivered_fetcher
            .get_payload_delivered(block_number)
            .await;
        if let Some(best_bid) = res.best_bid() {
            Ok(best_bid)
        } else {
            eyre::bail!(
                "No payload delivered for block: {}, relay_errors: {:?}",
                block_number,
                res.relay_errors
            );
        }
    }

    async fn get_onchain_block(&self, block_number: u64) -> eyre::Result<Block> {
        let block = self
            .eth_provider
            .get_block_by_number(BlockNumberOrTag::Number(block_number), true)
            .await
            .wrap_err_with(|| format!("Failed to fetch block {}", block_number))?
            .ok_or_else(|| eyre::eyre!("Block {} not found", block_number))?;
        Ok(block)
    }

    async fn fetch_mempool_txs(
        &self,
        block_number: u64,
        block_timestamp: u64,
    ) -> eyre::Result<Vec<OrdersWithTimestamp>> {
        let (from, to) = {
            let block_time = OffsetDateTime::from_unix_timestamp(block_timestamp as i64)?;
            (
                block_time - Duration::minutes(3),
                // we look ahead by 5 seconds in case block bid was delayed relative to the timestamp
                block_time + Duration::seconds(5),
            )
        };
        let mempool_txs = mempool::get_mempool_transactions(&self.mempool_datadir, from, to)
            .wrap_err_with(|| {
                format!(
                    "Failed to fetch mempool transactions for block {}",
                    block_number
                )
            })?;
        trace!(
            "Fetched unfiltered mempool transactions, count: {}",
            mempool_txs.len()
        );
        Ok(mempool_txs)
    }

    async fn fetch_bundles(
        &self,
        block_number: u64,
        block_timestamp: u64,
    ) -> eyre::Result<Vec<OrdersWithTimestamp>> {
        let db = if let Some(db) = &self.flashbots_db {
            RelayDB::new(db.clone())
        } else {
            info!("Flashbots db not set, skipping bundles");
            return Ok(Vec::new());
        };

        let bundles = db
            .get_simulated_bundles_for_block(block_number)
            .await
            .with_context(|| format!("Failed to fetch bundles for block {}", block_number))?;

        let block_timestamp = OffsetDateTime::from_unix_timestamp(block_timestamp as i64)?;
        let share_bundles = db
            .get_simulated_share_bundles_for_block(block_number, block_timestamp)
            .await
            .with_context(|| format!("Failed to fetch share bundles for block {}", block_number))?;

        trace!(
            "Fetched bundles from flashbots db, bundles: {}, sbundles: {}",
            bundles.len(),
            share_bundles.len()
        );

        Ok(bundles
            .into_iter()
            .chain(share_bundles.into_iter())
            .collect())
    }

    fn filter_orders_by_base_fee(
        &self,
        block_base_fee: u128,
        orders: &mut Vec<OrdersWithTimestamp>,
    ) {
        orders.retain(|order| {
            if !order.order.can_execute_with_block_base_fee(block_base_fee) {
                trace!("Order base fee too low, order: {:?}", order.order.id());
                false
            } else {
                true
            }
        })
    }

    async fn filter_order_by_nonces(
        &self,
        orders: Vec<OrdersWithTimestamp>,
        block_number: u64,
    ) -> eyre::Result<Vec<OrdersWithTimestamp>> {
        let nonces_to_check = orders
            .iter()
            .map(|o| (o.order.id(), o.order.nonces()))
            .collect::<Vec<_>>();

        let parent_block = block_number - 1;

        let nonce_cache = Arc::new(Mutex::new(HashMap::new()));
        let retain = Arc::new(Mutex::new(vec![false; nonces_to_check.len()]));

        let retain_clone = retain.clone();
        futures::stream::iter(nonces_to_check.into_iter().enumerate().map(Result::Ok))
            .try_for_each_concurrent(self.eth_rpc_parallel, move |(idx, (id, nonces))| {
                let nonce_cache = nonce_cache.clone();
                let retain_clone = retain_clone.clone();
                async move {
                    let mut all_nonces_failed = true;
                    for nonce in nonces {
                        let onchain_nonce = {
                            let mut nonce_cache = nonce_cache.lock().await;
                            if let Some(onchain_nonce) = nonce_cache.get(&nonce.address) {
                                *onchain_nonce
                            } else {
                                let address =
                                    alloy_primitives::Address::from_slice(&nonce.address.0 .0);
                                let onchain_nonce = self
                                    .eth_provider
                                    .get_transaction_count(address)
                                    .block_id(BlockId::Number(parent_block.into()))
                                    .await
                                    .wrap_err("Failed to fetch onchain tx count")?;
                                nonce_cache.insert(nonce.address, onchain_nonce);
                                onchain_nonce
                            }
                        };
                        if onchain_nonce > nonce.nonce && !nonce.optional {
                            trace!(
                                "Order nonce too low, order: {:?}, nonce: {}, onchain tx count: {}",
                                id,
                                nonce.nonce,
                                onchain_nonce,
                            );
                            return Ok(());
                        } else {
                            all_nonces_failed = false;
                        }
                    }

                    if all_nonces_failed {
                        trace!("All nonces failed, order: {:?}", id);
                        return Ok(());
                    }
                    trace!("Order nonce ok, order: {:?}", id);
                    let mut retain = retain_clone.lock().await;
                    retain[idx] = true;
                    Ok::<_, eyre::Error>(())
                }
            })
            .await?;

        let retain = retain.lock().await;
        Ok(orders
            .into_iter()
            .enumerate()
            .filter_map(|(idx, order)| if retain[idx] { Some(order) } else { None })
            .collect())
    }

    pub async fn fetch_historical_data(&self, block_number: u64) -> eyre::Result<BlockData> {
        info!("Fetching historical data for block {}", block_number);

        info!("Fetching payload delivered");
        let winning_bid_trace = self.get_payload_delivered_bid_trace(block_number).await?;

        info!("Fetching block from eth provider");
        let onchain_block = self.get_onchain_block(block_number).await?;

        let block_timestamp: u64 = timestamp_as_u64(&onchain_block);
        let mut orders = {
            let mut orders = self
                .fetch_mempool_txs(block_number, block_timestamp)
                .await?;
            let bundles = self.fetch_bundles(block_number, block_timestamp).await?;
            orders.extend(bundles);
            orders
        };

        info!("Fetched orders, unfiltered: {}", orders.len());

        let base_fee_per_gas = onchain_block.header.base_fee_per_gas.unwrap_or_default();
        self.filter_orders_by_base_fee(base_fee_per_gas, &mut orders);
        info!("Filtered orders by base fee, left: {}", orders.len());

        let mut available_orders = self.filter_order_by_nonces(orders, block_number).await?;
        info!(
            "Filtered orders by nonces, left: {}",
            available_orders.len()
        );
        available_orders.sort_by_key(|o| o.timestamp_ms);

        Ok(BlockData {
            block_number,
            winning_bid_trace,
            onchain_block,
            available_orders,
        })
    }
}
