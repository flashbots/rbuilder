use async_trait::async_trait;
use lru::LruCache;
use parking_lot::{Mutex, MutexGuard};
use std::{collections::HashMap, fmt::Debug, num::NonZeroUsize, sync::Arc, time::Duration};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
pub mod bids_publisher;
pub mod bloxroute_ws_publisher;
mod slot;
pub mod ultrasound_ws_publisher;

pub mod bid_sender;
pub mod code_from_rbuilder;
pub mod config;
pub mod headers_publisher;
pub mod types;
use types::BlockBid;

use crate::{bid_sender::BidSender, config::CfgWithSimpleRelayPublisherConfig};

pub type DynResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub const RPC_TIMEOUT: Duration = Duration::from_secs(60);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

pub fn get_timestamp_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
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

#[async_trait]
pub trait Service<CfgType: CfgWithSimpleRelayPublisherConfig>: Clone + Sized + Sync {
    fn inner(&self) -> MutexGuard<'_, ServiceInner<CfgType>>;
    fn cancellation_token(&self) -> CancellationToken;
    fn new_(
        name: String,
        sender: Arc<BidSender>,
        inner: Arc<Mutex<ServiceInner<CfgType>>>,
        cancel: CancellationToken,
    ) -> Self;
    // On error just return a string to log
    async fn new_blocks_subscriber(self) -> Result<(), String>;

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
                    error!(err, "Service::relay_subscriber failed. Cancelling.");
                    cancel.cancel();
                }
            });
        }

        if let Err(err) = Service::new_blocks_subscriber(self.clone()).await {
            error!(err, "new_blocks_subscriber failed. Cancelling.");
            self.cancellation_token().cancel();
        }
    }

    async fn relay_refresh(
        self,
        relay_name: String,
        relay_endpoint: String,
        bids_seen: Arc<Mutex<LruCache<BlockBid, ()>>>,
        client: Arc<reqwest::Client>,
    );

    async fn new<'a>(
        cfg: CfgType,
        name: String,
        sender: BidSender,
        cancel: CancellationToken,
    ) -> Self
    where
        CfgType: 'a,
    {
        let relays_file =
            std::fs::File::open(cfg.simple_relay_publisher_config().relays_file.clone())
                .expect("file should open read only");
        let relay_urls: HashMap<String, String> =
            serde_json::from_reader(relays_file).expect("file should be proper JSON");
        assert!(
            cfg.simple_relay_publisher_config().time_offset_index
                < cfg.simple_relay_publisher_config().time_offset_count
        );

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

        Self::new_(
            name,
            Arc::new(sender),
            Arc::new(Mutex::new(ServiceInner::<CfgType> {
                cfg,
                relays,
                last_block_number: 0,
                last_block_hash: String::new(),
                last_slot: 0,
                next_validator_pubkey: String::new(),
            })),
            cancel,
        )
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
    ) -> Result<(), String>
    where
        Self: 'static,
    {
        timeout(RPC_TIMEOUT, self.wait_until_ready(&cancellation_token))
            .await
            .map_err(|_| "Not ready after the timeout.")?;
        if cancellation_token.is_cancelled() {
            return Ok(());
        }

        let headers_seen: Arc<Mutex<LruCache<BlockBid, ()>>> =
            Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(4096).unwrap())));
        let client = Arc::new(
            reqwest::Client::builder()
                .user_agent("axios/0.27.2") // lulz
                .timeout(REQUEST_TIMEOUT)
                .build()
                .map_err(|_| "unable to build client.")?,
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
                return Err("We are at second {seconds_in_slot} in slot. Doesn't make sense. Is our node synced?".to_owned());
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
