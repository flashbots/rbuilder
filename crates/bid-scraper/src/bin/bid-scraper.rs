use alloy_primitives::{Address, BlockHash};
use alloy_rpc_types_beacon::BlsPublicKey;
use bid_scraper::best_bid_ws_connector::{
    BestBidValue, BestBidValueSink, BestBidWSConnector, ExternalWsPublisherConfig,
};
use bid_scraper::bid_sender::BidSender;
use bid_scraper::bids_publisher::{BidsPublisherService, RelayBidsPublisherConfig};
use bid_scraper::bloxroute_ws_publisher::{
    BloxrouteWsConnectionHandler, BloxrouteWsPublisher, BloxrouteWsPublisherConfig,
};
use bid_scraper::code_from_rbuilder::{
    load_config_toml_and_env, setup_tracing_subscriber, LoggerConfig,
};
use bid_scraper::config::{Config, PublisherConfig};
use bid_scraper::get_timestamp_f64;
use bid_scraper::headers_publisher::{HeadersPublisherService, RelayHeadersPublisherConfig};
use bid_scraper::types::{BlockBid, PublisherType};
use bid_scraper::ultrasound_ws_publisher::{
    UltrasoundWsConnectionHandler, UltrasoundWsPublisher, UltrasoundWsPublisherConfig,
};
use runng::protocol::Pub0;
use runng::Listen;
use std::env;
use std::time::Duration;
use tokio::signal::ctrl_c;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

trait PublisherFactory<CfgType, PublisherType> {
    async fn create_publisher(
        cfg: CfgType,
        name: String,
        sender: BidSender,
        cancel: CancellationToken,
    ) -> eyre::Result<PublisherType>;
    async fn run(publisher: PublisherType);
}

struct UltrasoundWsFactory;
impl PublisherFactory<UltrasoundWsPublisherConfig, UltrasoundWsPublisher> for UltrasoundWsFactory {
    async fn create_publisher(
        cfg: UltrasoundWsPublisherConfig,
        name: String,
        sender: BidSender,
        cancel: CancellationToken,
    ) -> eyre::Result<UltrasoundWsPublisher> {
        Ok(UltrasoundWsPublisher::new(
            UltrasoundWsConnectionHandler::new(cfg.clone(), name.clone()),
            sender,
            cancel,
        )
        .await)
    }
    async fn run(publisher: UltrasoundWsPublisher) {
        publisher.run().await
    }
}

struct BloxrouteWsFactory;
impl PublisherFactory<BloxrouteWsPublisherConfig, BloxrouteWsPublisher> for BloxrouteWsFactory {
    async fn create_publisher(
        cfg: BloxrouteWsPublisherConfig,
        name: String,
        sender: BidSender,
        cancel: CancellationToken,
    ) -> eyre::Result<BloxrouteWsPublisher> {
        Ok(BloxrouteWsPublisher::new(
            BloxrouteWsConnectionHandler::new(cfg.clone(), name.clone()),
            sender,
            cancel,
        )
        .await)
    }
    async fn run(publisher: BloxrouteWsPublisher) {
        publisher.run().await
    }
}

struct BidsPublisherServiceFactory;
impl PublisherFactory<RelayBidsPublisherConfig, BidsPublisherService>
    for BidsPublisherServiceFactory
{
    async fn create_publisher(
        cfg: RelayBidsPublisherConfig,
        name: String,
        sender: BidSender,
        cancel: CancellationToken,
    ) -> eyre::Result<BidsPublisherService> {
        <BidsPublisherService as bid_scraper::relay_api_publisher::Service<
            RelayBidsPublisherConfig,
        >>::new(cfg.clone(), name.clone(), sender, cancel)
        .await
    }
    async fn run(publisher: BidsPublisherService) {
        bid_scraper::relay_api_publisher::Service::run(publisher).await
    }
}

struct HeadersPublisherServiceFactory;
impl PublisherFactory<RelayHeadersPublisherConfig, HeadersPublisherService>
    for HeadersPublisherServiceFactory
{
    async fn create_publisher(
        cfg: RelayHeadersPublisherConfig,
        name: String,
        sender: BidSender,
        cancel: CancellationToken,
    ) -> eyre::Result<HeadersPublisherService> {
        <HeadersPublisherService as bid_scraper::relay_api_publisher::Service<
            RelayHeadersPublisherConfig,
        >>::new(cfg.clone(), name.clone(), sender, cancel)
        .await
    }
    async fn run(publisher: HeadersPublisherService) {
        bid_scraper::relay_api_publisher::Service::run(publisher).await
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        println!("Man, it's not that hard. It's a single parameter: the config file name. Something like:\n{} /home/cool_user_name/some_dir_to_keep_things_nice/some_other_dir_since_im_OCD/another_one/why_are_you_stil_reading_this_question_mark/stop_reading_and_fix_your_command_line/config_file.toml",args[0]);
        return Ok(());
    }

    let config: Config = load_config_toml_and_env(args[1].clone())?;

    let log_config = LoggerConfig {
        env_filter: config.log_level.clone(),
        log_json: config.log_json,
        log_color: config.log_color,
    };
    setup_tracing_subscriber(log_config)?;

    let global_cancel = CancellationToken::new();
    let global_cancel_clone = global_cancel.clone();
    let ctrlc = tokio::spawn(async move {
        ctrl_c().await.unwrap_or_default();
        global_cancel_clone.cancel()
    });

    let runng_factory = runng::factory::latest::ProtocolFactory::default();
    let mut nng_publisher_socket = runng_factory
        .publisher_open()
        .expect("unable to create NNG publisher");
    nng_publisher_socket
        .listen(&config.publisher_url)
        .expect("unable to have the NNG publisher listen");

    println!("{:?}", config.clone());
    for named_publisher in config.publishers {
        match named_publisher.publisher {
            PublisherConfig::RelayBids(cfg) => {
                tokio::spawn(start_publisher::<_, _, BidsPublisherServiceFactory>(
                    cfg,
                    named_publisher.name,
                    nng_publisher_socket.clone(),
                    global_cancel.clone(),
                ));
            }
            PublisherConfig::RelayHeaders(cfg) => {
                tokio::spawn(start_publisher::<_, _, HeadersPublisherServiceFactory>(
                    cfg,
                    named_publisher.name,
                    nng_publisher_socket.clone(),
                    global_cancel.clone(),
                ));
            }
            PublisherConfig::UltrasoundWs(cfg) => {
                tokio::spawn(start_publisher::<_, _, UltrasoundWsFactory>(
                    cfg,
                    named_publisher.name,
                    nng_publisher_socket.clone(),
                    global_cancel.clone(),
                ));
            }
            PublisherConfig::BloxrouteWs(cfg) => {
                tokio::spawn(start_publisher::<_, _, BloxrouteWsFactory>(
                    cfg,
                    named_publisher.name,
                    nng_publisher_socket.clone(),
                    global_cancel.clone(),
                ));
            }
            PublisherConfig::ExternalWs(external_ws_publisher_config) => {
                start_external_ws_publisher(
                    external_ws_publisher_config,
                    named_publisher.name,
                    nng_publisher_socket.clone(),
                    global_cancel.clone(),
                );
            }
        };
    }
    ctrlc.await.unwrap_or_default();
    Ok(())
}

/// How much time we wait when the creation of a publisher fails.
/// This should be a big value since unlikely that this get's fixed soon.
const WAIT_TIME_ON_CREATION_ERROR_SECS: u64 = 60;
/// How much time we wait when the run returns.
/// This usually happens on any unexpected error so the value should not be very high.
const WAIT_TIME_ON_RUN_ERROR_SECS: u64 = 10;

/// Start a publisher that will be restarted if it fails.
async fn start_publisher<CfgType, PublisherType, PublisherFactoryType>(
    cfg: CfgType,
    name: String,
    nng_publisher_socket: Pub0,
    global_cancel: CancellationToken,
) where
    CfgType: Clone,
    PublisherFactoryType: PublisherFactory<CfgType, PublisherType>,
{
    while !global_cancel.is_cancelled() {
        info!(name, "Initializing service...");
        let session_cancel = global_cancel.child_token();
        let sender = BidSender::new(
            nng_publisher_socket.clone(),
            global_cancel.clone(),
            session_cancel.clone(),
        );
        let timeout_secs = match PublisherFactoryType::create_publisher(
            cfg.clone(),
            name.clone(),
            sender,
            session_cancel,
        )
        .await
        {
            Ok(service) => {
                info!(name, "Service initialized!");
                PublisherFactoryType::run(service).await;
                info!(name, "Service died waiting to restart it");
                WAIT_TIME_ON_RUN_ERROR_SECS
            }
            Err(err) => {
                error!(err=?err, name, "Unable to create publisher");
                WAIT_TIME_ON_CREATION_ERROR_SECS
            }
        };
        let _ = timeout(Duration::from_secs(timeout_secs), global_cancel.cancelled()).await;
    }
}

/// Start a BestBidWSConnector which auto reconnects.
fn start_external_ws_publisher(
    external_ws_publisher_config: ExternalWsPublisherConfig,
    name: String,
    nng_publisher_socket: Pub0,
    global_cancel: CancellationToken,
) {
    let session_cancel = global_cancel.child_token();
    let sender = BidSender::new(
        nng_publisher_socket.clone(),
        global_cancel.clone(),
        session_cancel.clone(),
    );
    match create_best_bid_ws_connector(external_ws_publisher_config, sender, name.clone()) {
        Ok(ws_connector) => {
            tokio::spawn(async move { ws_connector.run_ws_stream(global_cancel).await });
        }
        Err(err) => {
            error!(?err, name, "Unable to create publisher");
        }
    }
}

struct BidSender2BestBidValueSink {
    sender: BidSender,
    name: String,
    fake_fee_recipient: Address,
    fake_builder_pubkey: BlsPublicKey,
}

impl BidSender2BestBidValueSink {
    fn new(sender: BidSender, name: String) -> Self {
        Self {
            sender,
            name,
            fake_fee_recipient: Address::random(),
            fake_builder_pubkey: BlsPublicKey::random(),
        }
    }
}

impl BestBidValueSink for BidSender2BestBidValueSink {
    fn send(&self, bid: BestBidValue) {
        let bid = BlockBid {
            seen_time: get_timestamp_f64(),
            publisher_name: self.name.clone(),
            publisher_type: PublisherType::ExternalWs,
            relay_time: None,
            relay_name: "external_ws_publisher".to_string(),
            block_hash: BlockHash::random(),
            block_number: bid.block_number,
            slot_number: bid.slot_number,
            parent_hash: bid.parent_hash,
            value: bid.block_top_bid,
            builder_pubkey: Some(self.fake_builder_pubkey),
            extra_data: None,
            fee_recipient: Some(self.fake_fee_recipient),
            proposer_fee_recipient: None,
            gas_used: None,
            optimistic_submission: None,
        };
        let _ = self.sender.send(bid);
    }
}

fn create_best_bid_ws_connector(
    external_ws_publisher_config: ExternalWsPublisherConfig,
    sender: BidSender,
    name: String,
) -> eyre::Result<BestBidWSConnector<BidSender2BestBidValueSink>> {
    BestBidWSConnector::new(
        &external_ws_publisher_config.url,
        &external_ws_publisher_config.auth_header.value()?,
        BidSender2BestBidValueSink::new(sender, name),
    )
}
