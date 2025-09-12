pub mod backtest_fetch;
pub mod data_source;
pub mod mempool;
pub mod mev_boost;

use crate::{
    backtest::{
        fetch::{
            data_source::{BlockRef, DataSource},
            mev_boost::PayloadDeliveredFetcher,
        },
        full_slot_block_data::{FullSlotBlockData, ReplaceableOrderPoolCommandWithTimestamp},
        BlockData, OrdersWithTimestamp,
    },
    mev_boost::BuilderBlockReceived,
    utils::timestamp_as_u64,
};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::{Block, BlockId, BlockNumberOrTag};
use eyre::Context;
use futures::TryStreamExt;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock},
};
use tokio::sync::Mutex;
use tracing::{info, trace, warn};

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
    eth_provider: RootProvider,
    eth_rpc_parallel: usize,
    data_sources: Vec<Box<dyn DataSource>>,
    payload_delivered_fetcher: PayloadDeliveredFetcher,
    require_delivered_payload_from_relays: bool,
}

impl HistoricalDataFetcher {
    pub fn new(eth_provider: RootProvider, eth_rpc_parallel: usize) -> Self {
        Self {
            eth_provider,
            eth_rpc_parallel,
            data_sources: vec![],
            payload_delivered_fetcher: PayloadDeliveredFetcher::default(),
            require_delivered_payload_from_relays: true,
        }
    }

    pub fn with_require_delivered_payload_from_relays(self, require: bool) -> Self {
        Self {
            require_delivered_payload_from_relays: require,
            ..self
        }
    }

    pub fn with_default_datasource(mut self, mempool_datadir: PathBuf) -> eyre::Result<Self> {
        let mempool = Box::new(mempool::MempoolDumpsterDatasource::new(mempool_datadir)?);
        self.data_sources.push(mempool);
        Ok(self)
    }

    pub fn with_datasource(mut self, datasource: Box<dyn DataSource>) -> Self {
        self.data_sources.push(datasource);
        self
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
            .get_block_by_number(BlockNumberOrTag::Number(block_number))
            .full()
            .await
            .wrap_err_with(|| format!("Failed to fetch block {block_number}"))?
            .ok_or_else(|| eyre::eyre!("Block {block_number} not found"))?;
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
    /// Also filters orders that will not fail but will execute nothing (eg: all optional already landed txs -> all txs will be skipped).
    async fn filter_order_by_nonces(
        &self,
        orders: Vec<OrdersWithTimestamp>,
        block_number: u64,
    ) -> eyre::Result<Vec<OrdersWithTimestamp>> {
        filter_order_by_nonces(
            orders,
            self.eth_rpc_parallel,
            &self.eth_provider,
            block_number,
        )
        .await
    }

    /// Fetches the non DataSource related data
    async fn fetch_basic_historical_data(
        &self,
        block_number: u64,
    ) -> eyre::Result<(BlockRef, Block, BuilderBlockReceived)> {
        info!(block_number, "Fetching historical data for block");

        info!("Fetching block from eth provider");
        let onchain_block = self.get_onchain_block(block_number).await?;

        let block_timestamp: u64 = timestamp_as_u64(&onchain_block);

        info!("Fetching payload delivered");
        let winning_bid_trace = match self.get_payload_delivered_bid_trace(block_number).await {
            Ok(res) => res,
            Err(err) => {
                if self.require_delivered_payload_from_relays {
                    return Err(err);
                } else {
                    warn!(
                        ?err,
                        "Failed to fetch payload from the relay, using fake payload"
                    );
                    generate_fake_delivered_payload(&onchain_block)
                }
            }
        };

        let block_ref = BlockRef::new(
            block_number,
            block_timestamp,
            Some(onchain_block.header.hash),
        );
        Ok((block_ref, onchain_block, winning_bid_trace))
    }

    pub async fn fetch_historical_data(&self, block_number: u64) -> eyre::Result<BlockData> {
        let (block_ref, onchain_block, winning_bid_trace) =
            self.fetch_basic_historical_data(block_number).await?;

        let mut orders: Vec<OrdersWithTimestamp> = vec![];
        let mut built_block_data = None;
        for datasource in &self.data_sources {
            let mut data = datasource.get_data(block_ref).await?;
            orders.append(&mut data.orders);
            if built_block_data.is_none() && data.built_block_data.is_some() {
                built_block_data = data.built_block_data;
            }
        }

        info!(count = orders.len(), "Fetched orders, unfiltered");

        let base_fee_per_gas = onchain_block.header.base_fee_per_gas.unwrap_or_default();
        self.filter_orders_by_base_fee(base_fee_per_gas as u128, &mut orders);
        info!(orders_left = orders.len(), "Filtered orders by base fee");

        let mut available_orders = self.filter_order_by_nonces(orders, block_number).await?;
        info!(
            orders_left = available_orders.len(),
            "Filtered orders by nonces"
        );
        available_orders.sort_by_key(|o| o.timestamp_ms);

        Ok(BlockData {
            block_number,
            winning_bid_trace,
            onchain_block,
            available_orders,
            built_block_data,
            filtered_orders: Default::default(),
        })
    }

    pub async fn fetch_full_slot_historical_data(
        &self,
        block_number: u64,
    ) -> eyre::Result<FullSlotBlockData> {
        let (block_ref, onchain_block, winning_bid_trace) =
            self.fetch_basic_historical_data(block_number).await?;

        let mut built_block_data = None;
        let mut available_orders: Vec<ReplaceableOrderPoolCommandWithTimestamp> = vec![];
        for datasource in &self.data_sources {
            let mut data = datasource.get_full_slot_data(block_ref).await?;
            available_orders.append(&mut data.orders);
            if built_block_data.is_none() && data.built_block_data.is_some() {
                built_block_data = data.built_block_data;
            }
        }
        info!(count = available_orders.len(), "Fetched available_orders");

        Ok(FullSlotBlockData::new(
            block_number,
            winning_bid_trace,
            onchain_block,
            available_orders,
            built_block_data,
        ))
    }
}

/// Filters out orders with non-optional sub txs (we can't skip them) already landed (onchain nonce > tx nonce, can't be re-executed!)
/// since they will fail.
/// Also filters orders that will not fail but will execute nothing (eg: all optional already landed txs -> all txs will be skipped).
pub async fn filter_order_by_nonces(
    orders: Vec<OrdersWithTimestamp>,
    eth_rpc_parallel: usize,
    eth_provider: &RootProvider,
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
        .try_for_each_concurrent(eth_rpc_parallel, move |(idx, (id, nonces))| {
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
                        let onchain_nonce = eth_provider
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

fn generate_fake_delivered_payload(block: &Block) -> BuilderBlockReceived {
    BuilderBlockReceived {
        slot: 0,
        parent_hash: block.header.parent_hash,
        block_hash: block.header.hash,
        builder_pubkey: Default::default(),
        proposer_pubkey: Default::default(),
        // insignificant random address
        proposer_fee_recipient: alloy_primitives::address!(
            "4e577795E42e1Df9234dc7F572aFd8Cc2777F739"
        ),
        gas_limit: block.header.gas_limit,
        gas_used: block.header.gas_used,
        value: Default::default(),
        num_tx: block.transactions.len() as u64,
        block_number: block.header.number,
        timestamp: block.header.timestamp,
        // assume that payload is generated 3 seconds after the slot timestamp
        timestamp_ms: (block.header.timestamp + 3) * 1000,
        optimistic_submission: false,
    }
}
