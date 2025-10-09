use crate::{
    bid_sender::BidSender,
    get_timestamp_f64,
    relay_api_publisher::{
        CfgWithSimpleRelayPublisherConfig, Service, ServiceInner, SimpleRelayPublisherConfig,
    },
    slot,
    types::{PublisherType, ScrapedRelayBlockBid},
    DynResult, RPC_TIMEOUT,
};
use alloy_primitives::{Address, BlockHash, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_beacon::BlsPublicKey;
use async_trait::async_trait;
use eyre::Context;
use lru::LruCache;
use parking_lot::{Mutex, MutexGuard};
use serde::Deserialize;
use std::{str::FromStr, sync::Arc};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RelayBidsPublisherConfig {
    #[serde(flatten)]
    pub simple_relay_cfg: SimpleRelayPublisherConfig,
}

impl CfgWithSimpleRelayPublisherConfig for RelayBidsPublisherConfig {
    fn simple_relay_publisher_config(&self) -> &SimpleRelayPublisherConfig {
        &self.simple_relay_cfg
    }
}

/// Publisher that scraps a relay by calling /relay/v1/data/bidtraces/builder_blocks_received
#[derive(Clone)]
pub struct BidsPublisherService {
    sender: Arc<dyn BidSender>,
    inner: Arc<Mutex<ServiceInner<RelayBidsPublisherConfig>>>,
    name: String,
    cancellation_token: CancellationToken,
}

#[async_trait]
impl Service<RelayBidsPublisherConfig> for BidsPublisherService {
    fn inner(&self) -> MutexGuard<'_, ServiceInner<RelayBidsPublisherConfig>> {
        trace!("mutex locking service");
        self.inner.lock()
    }

    fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    fn new_(
        name: String,
        sender: Arc<dyn BidSender>,
        inner: Arc<Mutex<ServiceInner<RelayBidsPublisherConfig>>>,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            sender,
            inner,
            name,
            cancellation_token,
        }
    }
    async fn relay_refresh(
        self,
        relay_name: String,
        relay_endpoint: String,
        bids_seen: Arc<Mutex<LruCache<ScrapedRelayBlockBid, ()>>>,
        client: Arc<reqwest::Client>,
    ) {
        let mut new_bids = 0;

        let bids = match self.get_bids(&relay_name, &relay_endpoint, &client).await {
            Ok(r) => r,
            Err(e) => {
                warn!("Error in get_bids for relay {}: {}", relay_name, e);
                return;
            }
        };

        for bid in bids {
            {
                let mut b = bids_seen.lock();
                if b.get(&bid).is_some() {
                    continue;
                }
                b.put(bid.clone(), ());
            }

            debug!("Got new bid from relay {}: {:?}", &relay_name, &bid);
            if self.sender.send(bid).is_err() {
                return;
            }
            new_bids += 1;
        }

        if new_bids > 0 {
            info!(
                "Sent {} bids from relay {} to subscribers.",
                new_bids, &relay_name
            );
        }
    }

    async fn new_blocks_subscriber(self) -> eyre::Result<()> {
        let eth_provider_uri = self
            .inner()
            .cfg
            .simple_relay_publisher_config()
            .eth_provider_uri
            .clone();
        let ws_conn = alloy_provider::WsConnect::new(eth_provider_uri);
        let provider = timeout(RPC_TIMEOUT, ProviderBuilder::new().connect_ws(ws_conn))
            .await
            .wrap_err("could not connect to node in time")?
            .wrap_err("unable to connect to node?")?;

        let mut subscription = provider
            .subscribe_blocks()
            .await
            .wrap_err("unable to subscribe to blocks")?;
        info!("New blocks subscriber connected and ready. Waiting for the first block...");
        let cancel_token = self.cancellation_token();
        while !cancel_token.is_cancelled() {
            let block = timeout(RPC_TIMEOUT, subscription.recv())
                .await
                .wrap_err("didn't receive a new block in time")?
                .wrap_err("didn't receive a new block")?;
            {
                trace!("got block {:?}", block);
                let mut inner = self.inner();
                inner.last_block_number = block.number;
                inner.last_block_hash = block.hash.to_string();
                inner.last_slot = slot::get_slot_number(block.timestamp);
                info!(
                    "New block {} ({}).",
                    inner.last_block_number, inner.last_block_hash,
                );
            }
        }
        Ok(())
    }
}

impl BidsPublisherService {
    async fn get_bids(
        &self,
        relay_name: &str,
        relay_endpoint: &str,
        client: &reqwest::Client,
    ) -> DynResult<Vec<ScrapedRelayBlockBid>> {
        debug!("Getting bids for relay {relay_name}");
        let block_number = self.inner().last_block_number + 1;
        let url = format!(
            "{relay_endpoint}/relay/v1/data/bidtraces/builder_blocks_received?block_number={block_number}&order_by=-value",
        );
        // By default it's ordered by slot (so, no effect). So we order by decreasing value
        // instead, it's more interesting to us.
        let response = client.get(url.clone()).send().await?;

        let status_code = response.status().as_u16();
        if status_code == 204 {
            return Ok(Vec::new());
        }
        if status_code == 400 || status_code == 429 {
            return Err(format!("HTTP {status_code}").into());
        }

        let mut json_bids: Vec<serde_json::Value> = response.json().await?;

        let mut bids = Vec::with_capacity(json_bids.len());
        for json_bid in json_bids.iter_mut() {
            let bid = ScrapedRelayBlockBid {
                publisher_name: self.name.clone(),
                publisher_type: PublisherType::RelayBids,
                builder_pubkey: Some(BlsPublicKey::from_str(
                    json_bid["builder_pubkey"]
                        .as_str()
                        .ok_or("unable to parse builder_pubkey")?,
                )?),
                relay_name: relay_name.to_string(),
                parent_hash: BlockHash::from_str(
                    json_bid["parent_hash"]
                        .as_str()
                        .ok_or("unable to parse parent_hash")?,
                )?,
                block_hash: BlockHash::from_str(
                    json_bid["block_hash"]
                        .as_str()
                        .ok_or("unable to parse block_hash")?,
                )?,
                seen_time: get_timestamp_f64(),
                relay_time: Some(if json_bid["timestamp_ms"].is_null() {
                    json_bid["timestamp"]
                        .as_str()
                        .ok_or("unable to parse timestamp")?
                        .parse::<u64>()? as f64
                } else {
                    json_bid["timestamp_ms"]
                        .as_str()
                        .ok_or("unable to parse timestamp_ms")?
                        .parse::<u64>()? as f64
                        / 1000.
                }),
                value: U256::from(
                    json_bid["value"]
                        .as_str()
                        .ok_or("unable to parse value")?
                        .parse::<u128>()?,
                ),
                slot_number: json_bid["slot"]
                    .as_str()
                    .ok_or("unable to parse slot")?
                    .parse::<u64>()?,
                gas_used: Some(
                    json_bid["gas_used"]
                        .as_str()
                        .ok_or("unable to parse gas_used")?
                        .parse::<u64>()?,
                ),
                proposer_fee_recipient: Some(Address::from_str(
                    json_bid["proposer_fee_recipient"]
                        .as_str()
                        .ok_or("unable to parse proposer_fee_recipient")?,
                )?),
                fee_recipient: None,
                optimistic_submission: json_bid["optimistic_submission"].as_bool(),
                block_number,
                extra_data: None,
            };
            debug!("Found bid: {bid:?}");
            bids.push(bid);
        }

        Ok(bids)
    }
}
