use crate::{
    best_bid_ws_connector::{
        BestBidValue, BestBidValueSink, BestBidWSConnector, ExternalWsPublisherConfig,
    },
    bid_sender::{BidSender, BidSenderCanceller},
    bids_publisher::{BidsPublisherService, RelayBidsPublisherConfig},
    bloxroute_ws_publisher::{
        BloxrouteWsConnectionHandler, BloxrouteWsPublisher, BloxrouteWsPublisherConfig,
    },
    config::{NamedPublisherConfig, PublisherConfig},
    get_timestamp_f64,
    headers_publisher::{HeadersPublisherService, RelayHeadersPublisherConfig},
    types::{PublisherType, ScrapedRelayBlockBid},
    ultrasound_ws_publisher::{
        UltrasoundWsConnectionHandler, UltrasoundWsPublisher, UltrasoundWsPublisherConfig,
    },
};
use alloy_primitives::{Address, BlockHash};
use alloy_rpc_types_beacon::BlsPublicKey;

use std::{sync::Arc, time::Duration};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

trait PublisherFactory<CfgType, PublisherType> {
    async fn create_publisher(
        cfg: CfgType,
        name: String,
        sender: Arc<dyn BidSender>,
        cancel: CancellationToken,
    ) -> eyre::Result<PublisherType>;
    async fn run(publisher: PublisherType);
}

struct UltrasoundWsFactory;
impl PublisherFactory<UltrasoundWsPublisherConfig, UltrasoundWsPublisher> for UltrasoundWsFactory {
    async fn create_publisher(
        cfg: UltrasoundWsPublisherConfig,
        name: String,
        sender: Arc<dyn BidSender>,
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
        sender: Arc<dyn BidSender>,
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
        sender: Arc<dyn BidSender>,
        cancel: CancellationToken,
    ) -> eyre::Result<BidsPublisherService> {
        <BidsPublisherService as crate::relay_api_publisher::Service<
            RelayBidsPublisherConfig,
        >>::new(cfg.clone(), name.clone(), sender, cancel)
        .await
    }
    async fn run(publisher: BidsPublisherService) {
        crate::relay_api_publisher::Service::run(publisher).await
    }
}

struct HeadersPublisherServiceFactory;
impl PublisherFactory<RelayHeadersPublisherConfig, HeadersPublisherService>
    for HeadersPublisherServiceFactory
{
    async fn create_publisher(
        cfg: RelayHeadersPublisherConfig,
        name: String,
        sender: Arc<dyn BidSender>,
        cancel: CancellationToken,
    ) -> eyre::Result<HeadersPublisherService> {
        <HeadersPublisherService as crate::relay_api_publisher::Service<
            RelayHeadersPublisherConfig,
        >>::new(cfg.clone(), name.clone(), sender, cancel)
        .await
    }
    async fn run(publisher: HeadersPublisherService) {
        crate::relay_api_publisher::Service::run(publisher).await
    }
}

pub fn run(
    publishers: Vec<NamedPublisherConfig>,
    sender: Arc<dyn BidSender>,
    global_cancel: CancellationToken,
) {
    for named_publisher in publishers {
        match named_publisher.publisher {
            PublisherConfig::RelayBids(cfg) => {
                tokio::spawn(start_publisher::<_, _, BidsPublisherServiceFactory>(
                    cfg,
                    named_publisher.name,
                    sender.clone(),
                    global_cancel.clone(),
                ));
            }
            PublisherConfig::RelayHeaders(cfg) => {
                tokio::spawn(start_publisher::<_, _, HeadersPublisherServiceFactory>(
                    cfg,
                    named_publisher.name,
                    sender.clone(),
                    global_cancel.clone(),
                ));
            }
            PublisherConfig::UltrasoundWs(cfg) => {
                tokio::spawn(start_publisher::<_, _, UltrasoundWsFactory>(
                    cfg,
                    named_publisher.name,
                    sender.clone(),
                    global_cancel.clone(),
                ));
            }
            PublisherConfig::BloxrouteWs(cfg) => {
                tokio::spawn(start_publisher::<_, _, BloxrouteWsFactory>(
                    cfg,
                    named_publisher.name,
                    sender.clone(),
                    global_cancel.clone(),
                ));
            }
            PublisherConfig::ExternalWs(external_ws_publisher_config) => {
                start_external_ws_publisher(
                    external_ws_publisher_config,
                    named_publisher.name,
                    sender.clone(),
                    global_cancel.clone(),
                );
            }
        };
    }
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
    sender: Arc<dyn BidSender>,
    global_cancel: CancellationToken,
) where
    CfgType: Clone,
    PublisherFactoryType: PublisherFactory<CfgType, PublisherType>,
{
    while !global_cancel.is_cancelled() {
        info!(name, "Initializing service...");
        let session_cancel = global_cancel.child_token();
        let sender = Arc::new(BidSenderCanceller::new(
            sender.clone(),
            session_cancel.clone(),
            global_cancel.clone(),
        ));
        let timeout_secs = match PublisherFactoryType::create_publisher(
            cfg.clone(),
            name.clone(),
            sender.clone(),
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
    sender: Arc<dyn BidSender>,
    global_cancel: CancellationToken,
) {
    let session_cancel = global_cancel.child_token();
    let sender = Arc::new(BidSenderCanceller::new(
        sender.clone(),
        session_cancel,
        global_cancel.clone(),
    ));
    match create_best_bid_ws_connector(external_ws_publisher_config, sender, name.clone()) {
        Ok(ws_connector) => {
            tokio::spawn(async move { ws_connector.run_ws_stream(global_cancel).await });
        }
        Err(err) => {
            error!(?err, name, "Unable to create publisher");
        }
    }
}

/// Simple adapter that creates BlockBids from BestBidValues and sends them to a BidSender.
struct BidSender2BestBidValueSink {
    sender: Arc<dyn BidSender>,
    name: String,
    fake_fee_recipient: Address,
    fake_builder_pubkey: BlsPublicKey,
}

impl BidSender2BestBidValueSink {
    fn new(sender: Arc<dyn BidSender>, name: String) -> Self {
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
        let bid = ScrapedRelayBlockBid {
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
    sender: Arc<dyn BidSender>,
    name: String,
) -> eyre::Result<BestBidWSConnector<BidSender2BestBidValueSink>> {
    BestBidWSConnector::new(
        &external_ws_publisher_config.url,
        &external_ws_publisher_config.auth_header.value()?,
        BidSender2BestBidValueSink::new(sender, name),
    )
}
