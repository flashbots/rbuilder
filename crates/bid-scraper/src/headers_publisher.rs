extern crate lru;
mod slot;
use async_trait::async_trait;
use clap::Parser;
use lru::LruCache;
use relay_bids_publisher::{
    get_timestamp_f64, types::BlockBid, Args, DynResult, Service, ServiceInner, REQUEST_TIMEOUT,
    RPC_TIMEOUT,
};
use std::{
    str::FromStr,
    sync::{Arc, Mutex, MutexGuard},
};
use tracing::{debug, info, trace, warn};

use ethers::{
    abi::{AbiDecode, AbiEncode},
    prelude::*,
};
use runng::{protocol::Pub0, SendSocket};
use tokio::time::timeout;

#[derive(Clone)]
pub struct HeadersPublisherService {
    nng_publisher_socket: Pub0,
    inner: Arc<Mutex<ServiceInner>>,
}

#[async_trait]
impl Service for HeadersPublisherService {
    fn inner(&self) -> MutexGuard<'_, ServiceInner> {
        trace!("mutex locking service");
        self.inner.lock().expect("service mutex poisoned")
    }

    fn new_(nng_publisher_socket: Pub0, inner: Arc<Mutex<ServiceInner>>) -> Self {
        Self {
            nng_publisher_socket,
            inner,
        }
    }

    async fn relay_refresh(
        self,
        relay_name: String,
        relay_endpoint: String,
        headers_seen: Arc<Mutex<LruCache<BlockBid, ()>>>,
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
            let mut h = headers_seen.lock().unwrap();
            if h.get(&header).is_some() {
                return;
            }
            h.put(header.clone(), ());
        }

        debug!("Got new header from relay {}: {:?}", &relay_name, &header);

        self.nng_publisher_socket
            .send(&serde_json::to_vec(&header).unwrap())
            .expect("unable to send message");
    }

    async fn new_blocks_subscriber(self) {
        let builder_uri = self.inner().args.builder_uri.clone();
        let provider = timeout(RPC_TIMEOUT, Provider::<Ws>::connect(builder_uri))
            .await
            .expect("could not connect to node in time")
            .expect("unable to connect to node?");
        let mut subscription = provider
            .subscribe_blocks()
            .await
            .expect("unable to subscribe to blocks");
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("unable to build client");

        info!("New blocks subscriber connected and ready. Waiting for the first block...");

        loop {
            let block = timeout(RPC_TIMEOUT, subscription.next())
                .await
                .expect("didn't receive a new block in time")
                .expect("didn't receive a new block");
            trace!("got block {:?}", block);
            let (beacon_node_uri, next_slot) = {
                let mut inner = self.inner();
                inner.last_block_number = block.number.expect("no block number").as_u64();
                inner.last_block_hash = block.hash.expect("no block hash").encode_hex();
                inner.last_slot = slot::get_slot_number(block.timestamp.as_u64());
                info!(
                    "New block {} ({}).",
                    inner.last_block_number, inner.last_block_hash,
                );
                (
                    inner
                        .args
                        .beacon_node_uri
                        .clone()
                        .expect("you need to set a beacon node uri!"),
                    inner.last_slot + 1,
                )
            };

            let duties: serde_json::Value = client
                .get(format!(
                    "{}/eth/v1/validator/duties/proposer/{}",
                    beacon_node_uri,
                    slot::get_epoch_number(next_slot),
                ))
                .send()
                .await
                .expect("Unable to fetch next validator duties")
                .json()
                .await
                .expect("unable to parse next validator duties");

            let next_validator_pubkeys: Vec<_> = duties["data"]
                .as_array()
                .expect("duties is not an array")
                .iter()
                .filter_map(|record| {
                    if record["slot"]
                        .as_str()
                        .expect("slot is not str")
                        .parse::<u64>()
                        .expect("unable to parse slot")
                        == next_slot
                    {
                        Some(record["pubkey"].as_str().expect("pubkey is not str"))
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(next_validator_pubkeys.len(), 1);
            self.inner().next_validator_pubkey = next_validator_pubkeys[0].to_owned();
        }
    }
}

impl HeadersPublisherService {
    async fn get_header(
        &self,
        relay_name: &str,
        relay_endpoint: &str,
        client: &reqwest::Client,
    ) -> DynResult<Option<BlockBid>> {
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
                "{}/eth/v1/builder/header/{next_slot}/{last_block_hash}/{next_validator_pubkey}",
                relay_endpoint,
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

        let header = BlockBid {
            publisher_name: self.inner().args.publisher_name.clone(),
            publisher_type: "headers".to_owned(),
            relay_name: relay_name.to_string(),
            slot_number: U64::from(next_slot),
            parent_hash: H256::decode_hex(
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
            block_hash: H256::decode_hex(
                msg["header"]["block_hash"]
                    .as_str()
                    .ok_or("block_hash not str")?,
            )?,
            block_number: Some(U64::from(
                msg["header"]["block_number"]
                    .as_str()
                    .ok_or("block_number not str")?
                    .parse::<u64>()?,
            )),
            extra_data: Some(
                msg["header"]["extra_data"]
                    .as_str()
                    .ok_or("extra_data not str")?
                    .to_owned(),
            ),
            gas_used: Some(U64::from(
                msg["header"]["gas_used"]
                    .as_str()
                    .ok_or("gas_used not str")?
                    .parse::<u64>()?,
            )),
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // when one task crashes we crash the whole program
    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        orig_hook(panic_info);
        std::process::exit(1);
    }));

    let args = Args::parse();

    info!("Initializing service...");
    let service = HeadersPublisherService::new(args.clone()).await;
    info!("Service initialized!");

    service.run().await;
}
