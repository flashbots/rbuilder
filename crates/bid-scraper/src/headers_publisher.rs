extern crate lru;
use crate::{
    bid_sender::BidSender,
    get_timestamp_f64,
    relay_api_publisher::{
        CfgWithSimpleRelayPublisherConfig, Service, ServiceInner, SimpleRelayPublisherConfig,
    },
    slot,
    types::{PublisherType, ScrapedRelayBlockBid},
    DynResult, REQUEST_TIMEOUT, RPC_TIMEOUT,
};
use alloy_primitives::{Address, BlockHash, U256};
use alloy_provider::{Provider, ProviderBuilder};
use async_trait::async_trait;
use eyre::{eyre, Context};
use lru::LruCache;
use parking_lot::{Mutex, MutexGuard};
use serde::Deserialize;
use std::{str::FromStr, sync::Arc};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RelayHeadersPublisherConfig {
    /// Endpoint for an CL client. Example:"ws://127.0.0.1:8545"
    pub beacon_node_uri: String,
    #[serde(flatten)]
    pub simple_relay_cfg: SimpleRelayPublisherConfig,
}

impl CfgWithSimpleRelayPublisherConfig for RelayHeadersPublisherConfig {
    fn simple_relay_publisher_config(&self) -> &SimpleRelayPublisherConfig {
        &self.simple_relay_cfg
    }
}

/// Publisher that scraps a relay by calling /eth/v1/builder/header/
#[derive(Clone)]
pub struct HeadersPublisherService {
    sender: Arc<dyn BidSender>,
    inner: Arc<Mutex<ServiceInner<RelayHeadersPublisherConfig>>>,
    name: String,
    cancellation_token: CancellationToken,
}

#[async_trait]
impl Service<RelayHeadersPublisherConfig> for HeadersPublisherService {
    fn inner(&self) -> MutexGuard<'_, ServiceInner<RelayHeadersPublisherConfig>> {
        trace!("mutex locking service");
        self.inner.lock()
    }

    fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    fn new_(
        name: String,
        sender: Arc<dyn BidSender>,
        inner: Arc<Mutex<ServiceInner<RelayHeadersPublisherConfig>>>,
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
        headers_seen: Arc<Mutex<LruCache<ScrapedRelayBlockBid, ()>>>,
        client: Arc<reqwest::Client>,
    ) {
        let header = match self.get_header(&relay_name, &relay_endpoint, &client).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return;
            }
            Err(e) => {
                warn!("Error in get_header for relay {}: {}", relay_name, e);
                return;
            }
        };

        {
            let mut h = headers_seen.lock();
            if h.get(&header).is_some() {
                return;
            }
            h.put(header.clone(), ());
        }

        debug!("Got new header from relay {}: {:?}", &relay_name, &header);

        let _ = self.sender.send(header);
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
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .wrap_err("unable to build client")?;

        info!("New blocks subscriber connected and ready. Waiting for the first block...");

        let cancel_token = self.cancellation_token();
        while !cancel_token.is_cancelled() {
            let block = timeout(RPC_TIMEOUT, subscription.recv())
                .await
                .wrap_err("didn't receive a new block in time")?
                .wrap_err("didn't receive a new block")?;
            trace!("got block {:?}", block);
            let (beacon_node_uri, next_slot) = {
                let mut inner = self.inner();
                inner.last_block_number = block.number;
                inner.last_block_hash = block.hash.to_string();
                inner.last_slot = slot::get_slot_number(block.timestamp);
                info!(
                    "New block {} ({}).",
                    inner.last_block_number, inner.last_block_hash,
                );
                (inner.cfg.beacon_node_uri.clone(), inner.last_slot + 1)
            };

            let duties: serde_json::Value = client
                .get(format!(
                    "{}/eth/v1/validator/duties/proposer/{}",
                    beacon_node_uri,
                    slot::get_epoch_number(next_slot),
                ))
                .send()
                .await
                .wrap_err("Unable to fetch next validator duties")?
                .json()
                .await
                .wrap_err("unable to parse next validator duties")?;

            let mut next_validator_pubkeys: Vec<&str> = Vec::new();
            for record in duties["data"]
                .as_array()
                .ok_or(eyre!("duties is not an array"))?
                .iter()
            {
                let slot = record["slot"]
                    .as_str()
                    .ok_or(eyre!("slot is not str"))?
                    .parse::<u64>()
                    .wrap_err("unable to parse slot")?;
                if slot == next_slot {
                    next_validator_pubkeys.push(
                        record["pubkey"]
                            .as_str()
                            .ok_or(eyre!("pubkey is not str"))?,
                    );
                }
            }
            if next_validator_pubkeys.len() != 1 {
                eyre::bail!("next_validator_pubkeys.len()!= 1");
            }
            self.inner().next_validator_pubkey = next_validator_pubkeys[0].to_owned();
        }
        Ok(())
    }
}

impl HeadersPublisherService {
    async fn get_header(
        &self,
        relay_name: &str,
        relay_endpoint: &str,
        client: &reqwest::Client,
    ) -> DynResult<Option<ScrapedRelayBlockBid>> {
        debug!("Getting header for relay {relay_name}");

        let (next_slot, last_block_hash, next_validator_pubkey) = {
            let inner = self.inner();
            (
                inner.last_slot + 1,
                inner.last_block_hash.clone(),
                inner.next_validator_pubkey.clone(),
            )
        };

        // By default it's ordered by slot (so, no effect). So we order by decreasing value
        // instead, it's more interesting to us.
        let response = client
            .get(format!(
                "{relay_endpoint}/eth/v1/builder/header/{next_slot}/{last_block_hash}/{next_validator_pubkey}",
            ))
            .send()
            .await?;

        let status_code = response.status().as_u16();
        if status_code == 204 {
            return Ok(None);
        }
        if status_code != 200 {
            return Err(format!("HTTP {status_code}").into());
        }

        let json_header: serde_json::Value = response.json().await?;

        debug!(
            "Got header response for relay {}: {}",
            relay_name, &json_header
        );

        let msg = &json_header["data"]["message"];

        let header = ScrapedRelayBlockBid {
            publisher_name: self.name.clone(),
            publisher_type: PublisherType::RelayHeaders,
            relay_name: relay_name.to_string(),
            slot_number: next_slot,
            parent_hash: BlockHash::from_str(
                msg["header"]["parent_hash"]
                    .as_str()
                    .ok_or("parent_hash not str")?,
            )?,
            proposer_fee_recipient: None,
            fee_recipient: Some(Address::from_str(
                msg["header"]["fee_recipient"]
                    .as_str()
                    .ok_or("fee_recipient not str")?,
            )?),
            block_hash: BlockHash::from_str(
                msg["header"]["block_hash"]
                    .as_str()
                    .ok_or("block_hash not str")?,
            )?,
            block_number: msg["header"]["block_number"]
                .as_str()
                .ok_or("block_number not str")?
                .parse::<u64>()?,
            extra_data: Some(
                msg["header"]["extra_data"]
                    .as_str()
                    .ok_or("extra_data not str")?
                    .to_owned(),
            ),
            gas_used: Some(
                msg["header"]["gas_used"]
                    .as_str()
                    .ok_or("gas_used not str")?
                    .parse::<u64>()?,
            ),
            value: U256::from(
                msg["value"]
                    .as_str()
                    .ok_or("value not str")?
                    .parse::<u128>()?,
            ),
            seen_time: get_timestamp_f64(),
            builder_pubkey: None,
            relay_time: None,
            optimistic_submission: None,
        };
        debug!("Found header: {header:?}");

        Ok(Some(header))
    }
}
