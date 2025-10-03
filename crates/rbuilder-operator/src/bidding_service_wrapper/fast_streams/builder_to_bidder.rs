use std::{
    sync::{mpsc, Arc},
    thread,
};

use iceoryx2::{
    node::{NodeBuilder, NodeCreationFailure},
    port::{
        listener::ListenerCreateError,
        notifier::NotifierCreateError,
        publisher::{Publisher, PublisherCreateError},
        subscriber::{Subscriber, SubscriberCreateError},
    },
    prelude::EventId,
    service::{
        builder::{
            event::EventOpenOrCreateError, publish_subscribe::PublishSubscribeOpenOrCreateError,
        },
        ipc,
        port_factory::{event, publish_subscribe},
        service_name::ServiceNameError,
    },
};
use rbuilder::{
    live_builder::block_output::bidding_service_interface::{
        BuiltBlockDescriptorForSlotBidder, ScrapedRelayBlockBidWithStats,
    },
    utils::sync::Watch,
};
use tokio_util::sync::CancellationToken;

use crate::bidding_service_wrapper::fast_streams::types::{
    BuiltBlockDescriptorForSlotBidderRPC, ScrapedRelayBlockBidRPC,
};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("NodeCreationFailure : {0}")]
    NodeCreationFailure(#[from] NodeCreationFailure),
    #[error("PublishSubscribeOpenOrCreateError : {0}")]
    PublishSubscribeOpenOrCreateError(#[from] PublishSubscribeOpenOrCreateError),
    #[error("ServiceNameError : {0}")]
    ServiceNameError(#[from] ServiceNameError),
    #[error("PublisherCreateError : {0}")]
    PublisherCreateError(#[from] PublisherCreateError),
    #[error("SubscriberCreateError : {0}")]
    SubscriberCreateError(#[from] SubscriberCreateError),
    #[error("EventOpenOrCreateError : {0}")]
    EventOpenOrCreateError(#[from] EventOpenOrCreateError),
    #[error("NotifierCreateError : {0}")]
    NotifierCreateError(#[from] NotifierCreateError),
    #[error("ListenerCreateError : {0}")]
    ListenerCreateError(#[from] ListenerCreateError),
}

pub type IceoryxScrapedBidsSubscriber = Subscriber<ipc::Service, ScrapedRelayBlockBidRPC, ()>;
pub type IceoryxScrapedBidsPublisher = Publisher<ipc::Service, ScrapedRelayBlockBidRPC, ()>;

const SCRAPED_BIDS_SERVICE_NAME: &str = "ScrapedBids";
const BLOCKS_SERVICE_NAME: &str = "NewBlocks";
const GOT_SCRAPED_BIDS_OR_BLOCKS_EVENT_NAME: &str = "GotScrapedBidsOrBlocksEvent";
const GOT_SCRAPED_BIDS_OR_BLOCKS_EVENT_ID: EventId = EventId::new(1usize);

/// Bids come at an aprox rate of 1000 per second. A whole second should be ok for the client to catch up even in the worst case.
pub const SCRAPED_BIDS_MAX_BUFFERS: usize = 1000;
/// New samples can eventually come from different scrapers each with it's own thread but we will never have more than 100 different scrapers.
const SCRAPED_MAX_LOAN_SAMPLES: usize = 100;

/// We only want new newest block.
pub const BLOCKS_MAX_BUFFERS: usize = 1;
/// Access should be sequential so a single buffer is enough.
const BLOCKS_MAX_LOAN_SAMPLES: usize = 2;

pub fn create_scraped_bids_service(
    node: &iceoryx2::node::Node<ipc::Service>,
) -> Result<publish_subscribe::PortFactory<ipc::Service, ScrapedRelayBlockBidRPC, ()>, Error> {
    Ok(node
        .service_builder(&SCRAPED_BIDS_SERVICE_NAME.try_into()?)
        .publish_subscribe::<ScrapedRelayBlockBidRPC>()
        .subscriber_max_buffer_size(SCRAPED_BIDS_MAX_BUFFERS)
        .open_or_create()?)
}

pub fn create_blocks_service(
    node: &iceoryx2::node::Node<ipc::Service>,
) -> Result<
    publish_subscribe::PortFactory<ipc::Service, BuiltBlockDescriptorForSlotBidderRPC, ()>,
    Error,
> {
    Ok(node
        .service_builder(&BLOCKS_SERVICE_NAME.try_into()?)
        .publish_subscribe::<BuiltBlockDescriptorForSlotBidderRPC>()
        .subscriber_max_buffer_size(BLOCKS_MAX_BUFFERS)
        .open_or_create()?)
}

pub fn create_got_scraped_bids_or_blocks_service(
    node: &iceoryx2::node::Node<ipc::Service>,
) -> Result<event::PortFactory<ipc::Service>, Error> {
    Ok(node
        .service_builder(&GOT_SCRAPED_BIDS_OR_BLOCKS_EVENT_NAME.try_into()?)
        .event()
        .open_or_create()?)
}

/// struct to publish ScrapedRelayBlockBidWithStats to the bidding service.
/// Adds an extra thread so we can call publisher code from a single thread since it's not Send.
#[derive(Debug)]
pub struct ScrapedBidsPublisher {
    scraped_bids_sender: mpsc::Sender<ScrapedRelayBlockBidRPC>,
}

impl ScrapedBidsPublisher {
    pub fn new() -> Self {
        let (scraped_bids_sender, scraped_bids_rx) = mpsc::channel::<ScrapedRelayBlockBidRPC>();
        thread::spawn(move || {
            let node = NodeBuilder::new().create::<ipc::Service>().unwrap();
            let scraped_bids_service = create_scraped_bids_service(&node).unwrap();
            let got_scraped_bids_or_blocks =
                create_got_scraped_bids_or_blocks_service(&node).unwrap();
            let publisher = scraped_bids_service
                .publisher_builder()
                .max_loaned_samples(SCRAPED_MAX_LOAN_SAMPLES)
                .create()
                .unwrap();
            let notifier = got_scraped_bids_or_blocks
                .notifier_builder()
                .create()
                .unwrap();
            while let Ok(scraped_bid) = scraped_bids_rx.recv() {
                let sample = publisher.loan_uninit().unwrap();
                let sample = sample.write_payload(scraped_bid);
                let _ = sample.send().unwrap();
                let _ = notifier
                    .notify_with_custom_event_id(GOT_SCRAPED_BIDS_OR_BLOCKS_EVENT_ID)
                    .unwrap();
            }
        });
        Self {
            scraped_bids_sender,
        }
    }

    pub fn send(&self, scraped_bid: ScrapedRelayBlockBidWithStats) {
        self.scraped_bids_sender
            .send(ScrapedRelayBlockBidRPC::from(scraped_bid))
            .unwrap();
    }
}

/// struct to publish BuiltBlockDescriptorForSlotBidder to the bidding service.
/// Adds an extra thread so we can call publisher code from a single thread since it's not Send.
/// @Pending: factorize with ScrapedBidsPublisher
#[derive(Debug)]
pub struct BlocksPublisher {
    last_block: Arc<Watch<BuiltBlockDescriptorForSlotBidder>>,
}

impl BlocksPublisher {
    pub fn new(session_id: u64, cancellation_token: CancellationToken) -> Self {
        let last_block: Arc<Watch<BuiltBlockDescriptorForSlotBidder>> = Arc::new(Watch::new());
        let last_block_clone = last_block.clone();
        thread::spawn(move || {
            let node = NodeBuilder::new().create::<ipc::Service>().unwrap();
            let blocks_service = create_blocks_service(&node).unwrap();
            let got_scraped_bids_or_blocks =
                create_got_scraped_bids_or_blocks_service(&node).unwrap();
            let publisher = blocks_service
                .publisher_builder()
                .max_loaned_samples(BLOCKS_MAX_LOAN_SAMPLES)
                .create()
                .unwrap();
            let notifier = got_scraped_bids_or_blocks
                .notifier_builder()
                .create()
                .unwrap();
            while !cancellation_token.is_cancelled() {
                if let Some(block) = last_block.wait_for_data() {
                    let sample = publisher.loan_uninit().unwrap();
                    let sample = sample.write_payload(BuiltBlockDescriptorForSlotBidderRPC::new(
                        session_id, block,
                    ));
                    let _ = sample.send().unwrap();
                    let _ = notifier
                        .notify_with_custom_event_id(GOT_SCRAPED_BIDS_OR_BLOCKS_EVENT_ID)
                        .unwrap();
                }
            }
        });
        Self {
            last_block: last_block_clone,
        }
    }

    pub fn send(&self, block: BuiltBlockDescriptorForSlotBidder) {
        self.last_block.set(block);
    }
}
