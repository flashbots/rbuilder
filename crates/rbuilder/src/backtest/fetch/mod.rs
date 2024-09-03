pub mod data_source;
pub mod flashbots_db;
pub mod mempool;
pub mod mev_boost;

use crate::{
    backtest::{
        fetch::data_source::{BlockRef, DataSource},
        BlockData,
    },
    mev_boost::BuilderBlockReceived,
    utils::timestamp_as_u64,
};

use alloy_provider::Provider;
use alloy_rpc_types::{Block, BlockId, BlockNumberOrTag};

use eyre::WrapErr;
use flashbots_db::RelayDB;
use futures::TryStreamExt;
use sqlx::PgPool;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock},
};
use tokio::sync::Mutex;
use tracing::{info, trace};

use crate::{
    backtest::{fetch::mev_boost::PayloadDeliveredFetcher, OrdersWithTimestamp},
    utils::BoxedProvider,
};

/// Struct that brings block information ([BlockData]) from several [DataSource]s
/// Filters txs already landed (onchain nonce > tx nonce)
/// Mainly used by [`backtest-fetch`]
/// Usage:
/// 1 - [HistoricalDataFetcher::new]
/// + (optional) [HistoricalDataFetcher::with_default_datasource]
/// + (optional) [HistoricalDataFetcher::with_datasource]
///
/// 2 - call [HistoricalDataFetcher::fetch_historical_data] for all the needed blocks
#[derive(Debug, Clone)]
pub struct HistoricalDataFetcher {
    eth_provider: BoxedProvider,
    eth_rpc_parallel: usize,
    data_sources: Vec<Box<dyn DataSource>>,
    payload_delivered_fetcher: PayloadDeliveredFetcher,
}

impl HistoricalDataFetcher {
    pub fn new(eth_provider: BoxedProvider, eth_rpc_parallel: usize) -> Self {
        Self {
            eth_provider,
            eth_rpc_parallel,
            data_sources: vec![],
            payload_delivered_fetcher: PayloadDeliveredFetcher::default(),
        }
    }

    pub fn with_default_datasource(
        mut self,
        mempool_datadir: PathBuf,
        flashbots_db: Option<PgPool>,
    ) -> eyre::Result<Self> {
        let mempool = Box::new(mempool::MempoolDumpsterDatasource::new(mempool_datadir)?);
        self.data_sources.push(mempool);

        if let Some(db_pool) = flashbots_db {
            let datasource = Box::new(RelayDB::new(db_pool));
            self.data_sources.push(datasource);
        }
        Ok(self)
    }

    pub fn with_datasource(self, datasource: Box<dyn DataSource>) -> Self {
        let mut data_sources = self.data_sources;
        data_sources.push(datasource);
        Self {
            data_sources,
            ..self
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

    /// Filters out orders with non-optional sub txs (we can't skip them) already landed (onchain nonce > tx nonce, can't be re-executed!)
    /// since they will fail.
    /// Also filters orders the will not fail but will execute nothing (eg: all optional already landed txs -> all txs will be skipped).
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

        let nonce_cache = Arc::new(RwLock::new(HashMap::new()));
        let retain = Arc::new(Mutex::new(vec![false; nonces_to_check.len()]));

        let retain_clone = retain.clone();
        futures::stream::iter(nonces_to_check.into_iter().enumerate().map(Result::Ok))
            .try_for_each_concurrent(self.eth_rpc_parallel, move |(idx, (id, nonces))| {
                let nonce_cache = nonce_cache.clone();
                let retain_clone = retain_clone.clone();
                async move {
                    let mut all_nonces_failed = true;
                    for nonce in nonces {
                        let mut res_onchain_nonce: Option<u64> = None;
                        if let Ok(nonce_cache) = nonce_cache.read() {
                            if let Some(onchain_nonce) = nonce_cache.get(&nonce.address) {
                                res_onchain_nonce = Some(*onchain_nonce);
                            }
                        }
                        let res_onchain_nonce = if let Some(res_onchain_nonce) = res_onchain_nonce {
                            res_onchain_nonce
                        } else {
                            let address = nonce.address;
                            let onchain_nonce = self
                                .eth_provider
                                .get_transaction_count(address)
                                .block_id(BlockId::Number(parent_block.into()))
                                .await
                                .wrap_err("Failed to fetch onchain tx count")?;

                            if let Ok(mut nonce_cache) = nonce_cache.write() {
                                nonce_cache.entry(address).or_insert(onchain_nonce);
                            }
                            onchain_nonce
                        };

                        if res_onchain_nonce > nonce.nonce && !nonce.optional {
                            trace!(
                                "Order nonce too low, order: {:?}, nonce: {}, onchain tx count: {}",
                                id,
                                nonce.nonce,
                                res_onchain_nonce,
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

        let mut orders: Vec<OrdersWithTimestamp> = vec![];
        let mut built_block_data = None;
        let block_ref = BlockRef::new(
            block_number,
            block_timestamp,
            Some(onchain_block.header.hash),
        );

        for datasource in &self.data_sources {
            let mut data = datasource.get_data(block_ref).await?;
            orders.append(&mut data.orders);
            if built_block_data.is_none() && data.built_block_data.is_some() {
                built_block_data = data.built_block_data;
            }
        }

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
            built_block_data,
        })
    }
}
