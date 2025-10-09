use std::{collections::HashMap, num::NonZeroUsize, sync::Arc, time::Duration};

use async_trait::async_trait;
use eyre::Context;
use lru::LruCache;
use parking_lot::{Mutex, MutexGuard};
use serde::Deserialize;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::{
    bid_sender::BidSender, get_timestamp_f64, slot, types::ScrapedRelayBlockBid, REQUEST_TIMEOUT,
    RPC_TIMEOUT,
};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SimpleRelayPublisherConfig {
    /// Endpoint for an EL client. Example:"ws://127.0.0.1:8545"
    pub eth_provider_uri: String,

    /// File containing a json list of relays like { "flashbots": "https://0xac6e77dfe25ecd6110b8e780608cce0dab71fdd5ebea22a16c0205200f2f8e2e3ad3b71d3499c54ad14d6c21b41a37ae@boost-relay.flashbots.net" }
    pub relays_file: String,
    /// Int between [0; time_offset_count) . We'll initiate our requests at exactly this time proportionally in the slot. Imagine you have 3 instances in 3 servers, you pass --time-offset-count 3 and then the first instance will have --time-offset-index 0, the second 1, and the third 2."
    pub time_offset_index: u64,
    pub time_offset_count: u64,
    /// When should start to query (in seconds) for bids in each slot. It's then shifted using time_offset_index/time_offset_count.
    pub request_start_s: f64,
    /// How often query for bids (in seconds), once we started.
    pub request_interval_s: f64,
    //#[clap(long, parse(try_from_str = try_parse_custom_request_interval), help="Override the request interval for a specific relay. Use like this: `--custom_request_interval relay_name=0.8`")]
    //pub custom_request_interval_s: Vec<(String, f64)>,
}

pub trait CfgWithSimpleRelayPublisherConfig: Send + Sync {
    fn simple_relay_publisher_config(&self) -> &SimpleRelayPublisherConfig;
}

#[derive(Debug, Clone)]
pub struct RelayParams {
    pub url: String,
    // when to start requesting, in each slot. It's specific to each relay and each job.
    pub request_start_s: f64,
    // how often to request, once we started. Specific to each relay (we can have custom request interval).
    pub request_interval_s: f64,
}

pub struct ServiceInner<CfgType> {
    pub cfg: CfgType,
    pub relays: HashMap<String, RelayParams>,
    pub last_block_number: u64,
    pub last_block_hash: String,
    pub last_slot: u64,
    pub next_validator_pubkey: String,
}

/// trait for publishers that call some API on relays.
#[async_trait]
pub trait Service<CfgType: CfgWithSimpleRelayPublisherConfig>: Clone + Sized + Sync {
    fn inner(&self) -> MutexGuard<'_, ServiceInner<CfgType>>;
    fn cancellation_token(&self) -> CancellationToken;
    fn new_(
        name: String,
        sender: Arc<dyn BidSender>,
        inner: Arc<Mutex<ServiceInner<CfgType>>>,
        cancel: CancellationToken,
    ) -> Self;
    // On error just return a string to log
    async fn new_blocks_subscriber(self) -> eyre::Result<()>;

    async fn run(self)
    where
        Self: 'static,
    {
        let relays = self.inner().relays.clone();
        for (relay_name, relay_params) in relays {
            let cancel = self.cancellation_token();
            let self_clone = self.to_owned();
            tokio::spawn(async move {
                if let Err(err) =
                    Service::relay_subscriber(self_clone, relay_name, relay_params, cancel.clone())
                        .await
                {
                    error!(err=?err, "Service::relay_subscriber failed. Cancelling.");
                    cancel.cancel();
                }
            });
        }

        if let Err(err) = Service::new_blocks_subscriber(self.clone()).await {
            error!(err=?err, "new_blocks_subscriber failed. Cancelling.");
            self.cancellation_token().cancel();
        }
    }

    async fn relay_refresh(
        self,
        relay_name: String,
        relay_endpoint: String,
        bids_seen: Arc<Mutex<LruCache<ScrapedRelayBlockBid, ()>>>,
        client: Arc<reqwest::Client>,
    );

    async fn new<'a>(
        cfg: CfgType,
        name: String,
        sender: Arc<dyn BidSender>,
        cancel: CancellationToken,
    ) -> eyre::Result<Self>
    where
        CfgType: 'a,
    {
        let relays_file =
            std::fs::File::open(cfg.simple_relay_publisher_config().relays_file.clone())
                .wrap_err("file should open read only")?;
        let relay_urls: HashMap<String, String> =
            serde_json::from_reader(relays_file).wrap_err("file should be proper JSON")?;
        if cfg.simple_relay_publisher_config().time_offset_index
            >= cfg.simple_relay_publisher_config().time_offset_count
        {
            eyre::bail!("time_offset_index >= time_offset_count");
        }

        let mut relays: HashMap<String, RelayParams> = HashMap::new();
        for (relay_name, relay_url) in relay_urls {
            let request_interval_s = cfg.simple_relay_publisher_config().request_interval_s;
            let request_start_s = cfg.simple_relay_publisher_config().request_start_s
                + request_interval_s
                    * (cfg.simple_relay_publisher_config().time_offset_index as f64
                        / cfg.simple_relay_publisher_config().time_offset_count as f64);
            info!(
                "Relay {}: start at {} seconds in slot, request every {} seconds.",
                relay_name, request_start_s, request_interval_s
            );
            relays.insert(
                relay_name,
                RelayParams {
                    url: relay_url,
                    request_start_s,
                    request_interval_s,
                },
            );
        }
        Ok(Self::new_(
            name,
            sender,
            Arc::new(Mutex::new(ServiceInner::<CfgType> {
                cfg,
                relays,
                last_block_number: 0,
                last_block_hash: String::new(),
                last_slot: 0,
                next_validator_pubkey: String::new(),
            })),
            cancel,
        ))
    }

    async fn wait_until_ready(&self, cancellation_token: &CancellationToken) {
        info!("Waiting for a new block...");
        while self.inner().last_slot == 0 {
            if timeout(Duration::from_millis(10), cancellation_token.cancelled())
                .await
                .is_ok()
            {
                return;
            }
        }
    }

    /// Loop until cancelled querying the relay for the bids via Self::relay_refresh.
    /// On error return a string.
    /// Does not call cancellation_token.cancel()
    async fn relay_subscriber(
        self,
        relay_name: String,
        relay_params: RelayParams,
        cancellation_token: CancellationToken,
    ) -> eyre::Result<()>
    where
        Self: 'static,
    {
        timeout(RPC_TIMEOUT, self.wait_until_ready(&cancellation_token))
            .await
            .wrap_err("Not ready after the timeout.")?;
        if cancellation_token.is_cancelled() {
            return Ok(());
        }

        let headers_seen: Arc<Mutex<LruCache<ScrapedRelayBlockBid, ()>>> =
            Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(4096).unwrap())));
        let client = Arc::new(
            reqwest::Client::builder()
                .user_agent("axios/0.27.2") // lulz
                .timeout(REQUEST_TIMEOUT)
                .build()
                .wrap_err("unable to build client.")?,
        );
        let request_interval = Duration::from_secs_f64(relay_params.request_interval_s);

        info!(
            "Relay subscriber for relay {} ready, entering main loop.",
            &relay_name,
        );

        while !cancellation_token.is_cancelled() {
            let start_timestamp = get_timestamp_f64();

            // This is so that we keep refreshing even if we are >12 seconds into the slot, some
            // validators may request late.
            let seconds_in_slot =
                slot::get_seconds_in_specific_slot(start_timestamp, self.inner().last_slot);
            if !(-1. ..600.).contains(&seconds_in_slot) {
                eyre::bail!("We are at second {seconds_in_slot} in slot. Doesn't make sense. Is our node synced?");
            }

            // requesting headers until 8/9 seconds into the block is useless
            // because 99% of blocks get mined after 9 seconds
            // so once slot changes we sleep until we can start requesting headers again
            if seconds_in_slot < relay_params.request_start_s {
                info!(
                    "No request needed yet for {}. Sleeping for {}.",
                    &relay_name,
                    relay_params.request_start_s - seconds_in_slot
                );
                // we sleep until we can start requesting headers
                let _ = timeout(
                    Duration::from_secs_f64(relay_params.request_start_s - seconds_in_slot),
                    cancellation_token.cancelled(),
                )
                .await;
                continue;
            }
            tokio::spawn(Self::relay_refresh(
                self.clone(),
                relay_name.clone(),
                relay_params.url.clone(),
                headers_seen.clone(),
                client.clone(),
            ));

            // sleep until we need to do our next request
            // this ensures we request exactly every `request_interval` seconds
            let _ = timeout(request_interval, cancellation_token.cancelled()).await;
        }
        Ok(())
    }
}
