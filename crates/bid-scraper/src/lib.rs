use async_trait::async_trait;
use clap::Parser;
use lru::LruCache;
use runng::{protocol::Pub0, Listen};
use std::{
    collections::HashMap,
    fmt::Debug,
    num::NonZeroUsize,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::time::timeout;
use tracing::info;
pub mod bids_publisher;
mod slot;

pub mod types;
use types::BlockBid;

pub type DynResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub const RPC_TIMEOUT: Duration = Duration::from_secs(60);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

pub fn get_timestamp_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn try_parse_custom_request_interval(s: &str) -> Result<(String, f64), clap::error::Error> {
    // TODO: return a proper clap error.
    let mut split = s.split('=');
    let relay_name = split
        .next()
        .expect("custom request interval format is relay_name=interval")
        .to_string();
    let interval = split
        .next()
        .expect("custom request interval format is relay_name=interval")
        .parse::<f64>()
        .expect("interval need to be a float");
    assert!(split.next().is_none());
    Ok((relay_name, interval))
}

#[derive(Parser, Clone)]
pub struct Args {
    #[clap(long)]
    pub eth_provider_uri: String,
    #[clap(long)]
    pub beacon_node_uri: Option<String>,
    #[clap(long, help = "Path to the relays file.")]
    pub relays_file: String,
    #[clap(
        long,
        help = "By default we publish bids for all relays in file, we can choose to exclude some. Specify relays to remove: e.g. `--exclude-relay flashbots --exclude-relay blocknative`"
    )]
    pub exclude_relay: Vec<String>,
    #[clap(long)]
    pub publisher_url: String,
    #[clap(long)]
    pub request_interval_s: f64,
    #[clap(
        long,
        help = "Int between [0; --time-offset-count[. We'll initiate our requests at exactly this time proportionally in the slot. Imagine you have 3 instances in 3 servers, you pass --time-offset-count 3 and then the first instance will have --time-offset-index 0, the second 1, and the third 2."
    )]
    pub time_offset_index: u64,
    #[clap(long)]
    pub time_offset_count: u64,
    #[clap(
        long,
        default_value = "6.0",
        help = "When these jobs should start to query for bids, in each slot. It's then shifted using time_offset_index/time_offset_count."
    )]
    pub request_start_s: f64,
    #[clap(long, parse(try_from_str = try_parse_custom_request_interval), help="Override the request interval for a specific relay. Use like this: `--custom_request_interval relay_name=0.8`")]
    pub custom_request_interval_s: Vec<(String, f64)>,
    #[clap(long)]
    pub publisher_name: String,
}

#[derive(Debug, Clone)]
pub struct RelayParams {
    pub url: String,
    // when to start requesting, in each slot. It's specific to each relay and each job.
    pub request_start_s: f64,
    // how often to request, once we started. Specific to each relay (we can have custom request interval).
    pub request_interval_s: f64,
}

pub struct ServiceInner {
    pub args: Args,
    pub relays: HashMap<String, RelayParams>,
    pub last_block_number: u64,
    pub last_block_hash: String,
    pub last_slot: u64,
    pub next_validator_pubkey: String,
}

#[async_trait]
pub trait Service: Clone + Sized + Sync {
    fn inner(&self) -> MutexGuard<'_, ServiceInner>;
    fn new_(nng_publisher_socket: Pub0, inner: Arc<Mutex<ServiceInner>>) -> Self;
    async fn new_blocks_subscriber(self);

    async fn run(self)
    where
        Self: 'static,
    {
        for relay_name in self.inner().relays.keys() {
            tokio::spawn(Service::relay_subscriber(
                self.to_owned(),
                relay_name.clone(),
            ));
        }

        Service::new_blocks_subscriber(self.clone()).await;
    }

    async fn relay_refresh(
        self,
        relay_name: String,
        relay_endpoint: String,
        bids_seen: Arc<Mutex<LruCache<BlockBid, ()>>>,
        client: Arc<reqwest::Client>,
    );

    async fn new(args: Args) -> Self {
        let runng_factory = runng::factory::latest::ProtocolFactory::default();
        let mut nng_publisher_socket = runng_factory
            .publisher_open()
            .expect("unable to create NNG publisher");
        nng_publisher_socket
            .listen(&args.publisher_url)
            .expect("unable to have the NNG publisher listen");

        let relays_file =
            std::fs::File::open(args.relays_file.clone()).expect("file should open read only");
        let mut relay_urls: HashMap<String, String> =
            serde_json::from_reader(relays_file).expect("file should be proper JSON");

        for relay_name in args.exclude_relay.iter() {
            assert!(relay_urls.remove(relay_name).is_some());
        }
        assert!(
            !relay_urls.is_empty(),
            "You need to pass at least one relay."
        );

        let relay_custom_interval_s: HashMap<String, f64> =
            HashMap::from_iter(args.custom_request_interval_s.iter().cloned());
        for relay_name in relay_custom_interval_s.keys() {
            assert!(
                relay_urls.contains_key(relay_name),
                "You specified a custom interval for relay {relay_name} but we don't have it!"
            );
        }

        assert!(args.time_offset_index < args.time_offset_count);

        let mut relays: HashMap<String, RelayParams> = HashMap::new();
        for (relay_name, relay_url) in relay_urls {
            let request_interval_s = relay_custom_interval_s
                .get(&relay_name)
                .cloned()
                .unwrap_or(args.request_interval_s);
            let request_start_s = args.request_start_s
                + request_interval_s
                    * (args.time_offset_index as f64 / args.time_offset_count as f64);
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
            nng_publisher_socket,
            Arc::new(Mutex::new(ServiceInner {
                args,
                relays,
                last_block_number: 0,
                last_block_hash: String::new(),
                last_slot: 0,
                next_validator_pubkey: String::new(),
            })),
        )
    }

    async fn wait_until_ready(&self) {
        info!("Waiting for a new block...");
        while self.inner().last_slot == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn relay_subscriber(self, relay_name: String)
    where
        Self: 'static,
    {
        timeout(RPC_TIMEOUT, self.wait_until_ready())
            .await
            .expect("Not ready after the timeout.");

        let headers_seen: Arc<Mutex<LruCache<BlockBid, ()>>> =
            Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(4096).unwrap())));
        let client = Arc::new(
            reqwest::Client::builder()
                .user_agent("axios/0.27.2") // lulz
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("unable to build client"),
        );
        let relay_params = self.inner().relays.get(&relay_name).unwrap().clone();
        let request_interval = Duration::from_secs_f64(relay_params.request_interval_s);

        info!(
            "Relay subscriber for relay {} ready, entering main loop.",
            &relay_name,
        );

        loop {
            let start_timestamp = get_timestamp_f64();

            // This is so that we keep refreshing even if we are >12 seconds into the slot, some
            // validators may request late.
            let seconds_in_slot =
                slot::get_seconds_in_specific_slot(start_timestamp, self.inner().last_slot);
            assert!(
                (-1. ..600.).contains(&seconds_in_slot),
                "We are at second {seconds_in_slot} in slot. Doesn't make sense. Is our node synced?"
            );

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
                tokio::time::sleep(Duration::from_secs_f64(
                    relay_params.request_start_s - seconds_in_slot,
                ))
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
            tokio::time::sleep(request_interval).await;
        }
    }
}
