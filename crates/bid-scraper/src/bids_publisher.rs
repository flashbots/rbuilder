use crate::{
    get_timestamp_f64, slot, types::BlockBid, Args, DynResult, Service, ServiceInner, RPC_TIMEOUT,
};
use async_trait::async_trait;
use clap::Parser;
use ethers::{
    abi::{AbiDecode, AbiEncode},
    prelude::*,
};
use lru::LruCache;
use runng::{protocol::Pub0, SendSocket};
use std::{
    str::FromStr,
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::time::timeout;
use tracing::{debug, info, trace, warn};

#[derive(Clone)]
pub struct BidsPublisherService {
    nng_publisher_socket: Pub0,
    inner: Arc<Mutex<ServiceInner>>,
}

#[async_trait]
impl Service for BidsPublisherService {
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
        bids_seen: Arc<Mutex<LruCache<BlockBid, ()>>>,
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
                let mut b = bids_seen.lock().unwrap();
                if b.get(&bid).is_some() {
                    continue;
                }
                b.put(bid.clone(), ());
            }

            debug!("Got new bid from relay {}: {:?}", &relay_name, &bid);

            self.nng_publisher_socket
                .send(&serde_json::to_vec(&bid).unwrap())
                .expect("unable to send message");

            new_bids += 1;
        }

        if new_bids > 0 {
            info!(
                "Sent {} bids from relay {} to subscribers.",
                new_bids, &relay_name
            );
        }
    }

    async fn new_blocks_subscriber(self) {
        let eth_provider_uri = self.inner().args.eth_provider_uri.clone();
        let provider = timeout(RPC_TIMEOUT, Provider::<Ws>::connect(eth_provider_uri))
            .await
            .expect("could not connect to node in time")
            .expect("unable to connect to node?");
        let mut subscription = provider
            .subscribe_blocks()
            .await
            .expect("unable to subscribe to blocks");

        info!("New blocks subscriber connected and ready. Waiting for the first block...");

        loop {
            let block = timeout(RPC_TIMEOUT, subscription.next())
                .await
                .expect("didn't receive a new block in time")
                .expect("didn't receive a new block");
            {
                trace!("got block {:?}", block);
                let mut inner = self.inner();
                inner.last_block_number = block.number.expect("no block number").as_u64();
                inner.last_block_hash = block.hash.expect("no block hash").encode_hex();
                inner.last_slot = slot::get_slot_number(block.timestamp.as_u64());
                info!(
                    "New block {} ({}).",
                    inner.last_block_number, inner.last_block_hash,
                );
            }
        }
    }
}

impl BidsPublisherService {
    async fn get_bids(
        &self,
        relay_name: &str,
        relay_endpoint: &str,
        client: &reqwest::Client,
    ) -> DynResult<Vec<BlockBid>> {
        debug!("Getting bids for relay {relay_name}");

        let url = format!(
            "{}/relay/v1/data/bidtraces/builder_blocks_received?block_number={}&order_by=-value",
            relay_endpoint,
            self.inner().last_block_number + 1
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
        info!(url, "----------------BIDS---------------");
        for json_bid in json_bids.iter_mut() {
            info!(json_bid=?json_bid,"BID");
            let bid = BlockBid {
                publisher_name: self.inner().args.publisher_name.clone(),
                publisher_type: "bids".to_owned(),
                builder_pubkey: Some(
                    json_bid["builder_pubkey"]
                        .as_str()
                        .ok_or("unable to parse builder_pubkey")?
                        .to_lowercase(),
                ),
                relay_name: relay_name.to_string(),
                parent_hash: H256::decode_hex(
                    json_bid["parent_hash"]
                        .as_str()
                        .ok_or("unable to parse parent_hash")?,
                )?,
                block_hash: H256::decode_hex(
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
                slot_number: U64::from(
                    json_bid["slot"]
                        .as_str()
                        .ok_or("unable to parse slot")?
                        .parse::<u64>()?,
                ),
                gas_used: Some(U64::from(
                    json_bid["gas_used"]
                        .as_str()
                        .ok_or("unable to parse gas_used")?
                        .parse::<u64>()?,
                )),
                proposer_fee_recipient: Some(Address::from_str(
                    json_bid["proposer_fee_recipient"]
                        .as_str()
                        .ok_or("unable to parse proposer_fee_recipient")?,
                )?),
                fee_recipient: None,
                optimistic_submission: json_bid["optimistic_submission"].as_bool(),
                block_number: None,
                extra_data: None,
            };
            debug!("Found bid: {bid:?}");
            bids.push(bid);
        }

        Ok(bids)
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
    let service = BidsPublisherService::new(args.clone()).await;
    info!("Service initialized!");

    service.run().await;
}
