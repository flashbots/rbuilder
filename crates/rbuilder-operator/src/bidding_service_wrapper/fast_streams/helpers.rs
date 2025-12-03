use std::{
    collections::HashMap,
    sync::{mpsc, Arc},
    thread,
};

use iceoryx2::{
    config::Config,
    node::{Node, NodeBuilder, NodeCreationFailure},
    port::{
        listener::{Listener, ListenerCreateError},
        notifier::{Notifier, NotifierCreateError, NotifierNotifyError},
        publisher::{Publisher, PublisherCreateError},
        subscriber::{Subscriber, SubscriberCreateError},
        LoanError, ReceiveError, SendError,
    },
    prelude::{SignalHandlingMode, ZeroCopySend},
    service::{
        builder::{
            event::EventOpenOrCreateError, publish_subscribe::PublishSubscribeOpenOrCreateError,
        },
        ipc,
        port_factory::{event, publish_subscribe},
        service_name::ServiceNameError,
    },
};
use parking_lot::Mutex;
use rbuilder::{
    live_builder::block_output::bidding_service_interface::{
        BlockSealInterfaceForSlotBidder, RelaySet, ScrapedRelayBlockBidWithStats,
    },
    utils::sync::{Watch, THREAD_BLOCKING_DURATION},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::bidding_service_wrapper::fast_streams::{
    subscriber_poller::SubscriberPoller,
    types::{
        BuiltBlockDescriptorForSlotBidderRPC, BuiltBlockDescriptorForSlotBidderWithSessionId,
        ScrapedRelayBlockBidRPC, SlotBidderSealBidCommandRPC,
        SlotBidderSealBidCommandWithSessionId,
    },
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
    #[error("LoanError : {0}")]
    LoanError(#[from] LoanError),
    #[error("SendError : {0}")]
    SendError(#[from] SendError),
    #[error("NotifierNotifyError : {0}")]
    NotifierNotifyError(#[from] NotifierNotifyError),
    #[error("ReceiveError : {0}")]
    ReceiveError(#[from] ReceiveError),
}

pub type IceoryxScrapedBidsSubscriber = Subscriber<ipc::Service, ScrapedRelayBlockBidRPC, ()>;
pub type IceoryxScrapedBidsPublisher = Publisher<ipc::Service, ScrapedRelayBlockBidRPC, ()>;

const SCRAPED_BIDS_SERVICE_NAME: &str = "ScrapedBidsService";
const BLOCKS_SERVICE_NAME: &str = "NewBlocksService";
const SLOT_BIDDER_SEAL_BID_COMMAND_SERVICE_NAME: &str = "SlotBidderSealBidCommandService";
const GOT_SLOT_BIDDER_SEAL_BID_COMMAND_EVENT_NAME: &str = "GotSlotBidderSealBidCommandEvent";
const GOT_SCRAPED_BIDS_OR_BLOCKS_EVENT_NAME: &str = "GotScrapedBidsOrBlocksEvent";

/// Bids come at an aprox rate of 1000 per second. A whole second should be ok for the client to catch up even in the worst case.
pub const SCRAPED_BIDS_MAX_BUFFERS: usize = 1000;
/// New samples can eventually come from different scrapers each with it's own thread but we will never have more than 100 different scrapers.
const SCRAPED_MAX_LOAN_SAMPLES: usize = 100;

/// IMPORTANT: MAX_PUBLISHERS must be >= 2 since with 1, if the process dies, we've seen the connection fail for ever. We choose 3 to be safe.
/// We also chose 3 instead of 1 to be safe with the MAX_SUBSCRIBERS.
/// Should have only a single publisher.
const BLOCKS_SERVICE_MAX_PUBLISHERS: usize = 3;
/// Should have only a single subscriber.
const BLOCKS_SERVICE_MAX_SUBSCRIBERS: usize = 3;

/// Should have only a single publisher.
const SCRAPED_BIDS_SERVICE_MAX_PUBLISHERS: usize = 3;
/// Should have only a single subscriber..
const SCRAPED_BIDS_SERVICE_MAX_SUBSCRIBERS: usize = 3;

/// We only want newest item.
pub const LAST_ITEM_MAX_BUFFERS: usize = 1;
/// Access should be sequential so a single buffer is enough.
pub const LAST_ITEM_MAX_LOAN_SAMPLES: usize = 2;

/// We create a publisher for active block. We usually can have 2 (prev and current) but with forks we could have more.
/// I don't think we would ever have more than 5 but we play it safe.
pub const SLOT_BIDDER_SEAL_BID_COMMAND_SERVICE_MAX_PUBLISHERS: usize = 10;
/// We should only have a single subscriber to the slot bidder seal bid command service since we spawn a single thread to poll for them.
pub const SLOT_BIDDER_SEAL_BID_COMMAND_SERVICE_MAX_SUBSCRIBERS: usize = 3;

pub const SERVICE_MAX_PUBLISHERS: usize = 10;
pub const SERVICE_MAX_SUBSCRIBERS: usize = 10;

/// Always use this function to create a node builder to avoid issues with signal handling.
pub fn create_node_builder() -> Result<Node<ipc::Service>, Error> {
    let mut config = Config::global_config().clone();
    config.defaults.publish_subscribe.max_publishers = SERVICE_MAX_PUBLISHERS;
    config.defaults.publish_subscribe.max_subscribers = SERVICE_MAX_SUBSCRIBERS;
    Ok(NodeBuilder::new()
        .config(&config)
        .signal_handling_mode(SignalHandlingMode::Disabled)
        .create::<ipc::Service>()?)
}

pub fn create_scraped_bids_service(
    node: &iceoryx2::node::Node<ipc::Service>,
) -> Result<publish_subscribe::PortFactory<ipc::Service, ScrapedRelayBlockBidRPC, ()>, Error> {
    Ok(node
        .service_builder(&SCRAPED_BIDS_SERVICE_NAME.try_into()?)
        .publish_subscribe::<ScrapedRelayBlockBidRPC>()
        .subscriber_max_buffer_size(SCRAPED_BIDS_MAX_BUFFERS)
        .max_publishers(SCRAPED_BIDS_SERVICE_MAX_PUBLISHERS)
        .max_subscribers(SCRAPED_BIDS_SERVICE_MAX_SUBSCRIBERS)
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
        .subscriber_max_buffer_size(LAST_ITEM_MAX_BUFFERS)
        .max_publishers(BLOCKS_SERVICE_MAX_PUBLISHERS)
        .max_subscribers(BLOCKS_SERVICE_MAX_SUBSCRIBERS)
        .open_or_create()?)
}

pub fn create_slot_bidder_seal_bid_command_service(
    node: &iceoryx2::node::Node<ipc::Service>,
) -> Result<publish_subscribe::PortFactory<ipc::Service, SlotBidderSealBidCommandRPC, ()>, Error> {
    Ok(node
        .service_builder(&SLOT_BIDDER_SEAL_BID_COMMAND_SERVICE_NAME.try_into()?)
        .publish_subscribe::<SlotBidderSealBidCommandRPC>()
        .subscriber_max_buffer_size(LAST_ITEM_MAX_BUFFERS)
        .max_publishers(SLOT_BIDDER_SEAL_BID_COMMAND_SERVICE_MAX_PUBLISHERS)
        .max_subscribers(SLOT_BIDDER_SEAL_BID_COMMAND_SERVICE_MAX_SUBSCRIBERS)
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

pub fn create_got_slot_bidder_seal_bid_command_event_service(
    node: &iceoryx2::node::Node<ipc::Service>,
) -> Result<event::PortFactory<ipc::Service>, Error> {
    Ok(node
        .service_builder(&GOT_SLOT_BIDDER_SEAL_BID_COMMAND_EVENT_NAME.try_into()?)
        .event()
        .open_or_create()?)
}

/// iceoryx published + event to notify on every new item we publish.
/// Just create and call send.
struct NotifyingPublisher<ItemTypeRPC: std::fmt::Debug + ZeroCopySend + 'static> {
    publisher: Publisher<ipc::Service, ItemTypeRPC, ()>,
    notifier: Notifier<ipc::Service>,
}

impl<ItemTypeRPC: std::fmt::Debug + ZeroCopySend + 'static> NotifyingPublisher<ItemTypeRPC> {
    /// item_service_name: name of the service to publish the item to (the subscriber must match the publisher).
    /// got_item_event_name: name of the event to notify on every new item (the subscriber must match the publisher)..
    /// max_subscriber_buffers: max number of buffers to keep. It the subscriber does not poll fast enough some items will be lost. If you only care about the last item just use 1.
    /// max_publisher_loan_buffers: max number of buffers to loan to all publisher instances. This should be >= number of publishers instances.
    pub fn new(
        item_service_name: &'static str,
        got_item_event_name: &'static str,
        max_subscriber_buffers: usize,
        max_publisher_loan_buffers: usize,
        max_publishers: usize,
        max_subscribers: usize,
    ) -> Result<Self, Error> {
        let node = create_node_builder()?;
        let item_service = node
            .service_builder(&item_service_name.try_into()?)
            .publish_subscribe::<ItemTypeRPC>()
            .max_publishers(max_publishers)
            .max_subscribers(max_subscribers)
            .subscriber_max_buffer_size(max_subscriber_buffers)
            .open_or_create()?;
        let got_item = node
            .service_builder(&got_item_event_name.try_into()?)
            .event()
            .open_or_create()?;
        let publisher = item_service
            .publisher_builder()
            .max_loaned_samples(max_publisher_loan_buffers)
            .create()?;
        let notifier = got_item.notifier_builder().create()?;
        Ok(Self {
            publisher,
            notifier,
        })
    }

    pub fn send(&self, item: ItemTypeRPC) -> Result<(), Error> {
        let sample = self.publisher.loan_uninit()?;
        let sample = sample.write_payload(item);
        sample.send()?;
        self.notifier.notify()?;
        Ok(())
    }
}

/// struct to publish ScrapedRelayBlockBidWithStats to the bidding service.
/// Adds an extra thread so we can call publisher code from a single thread (since it's not Send).
#[derive(Debug)]
pub struct ScrapedBidsPublisher {
    scraped_bids_sender: mpsc::Sender<ScrapedRelayBlockBidRPC>,
}

impl ScrapedBidsPublisher {
    pub fn new() -> Result<Self, Error> {
        let (scraped_bids_sender, scraped_bids_rx) = mpsc::channel::<ScrapedRelayBlockBidRPC>();
        let init_done = Arc::new(Watch::<Result<(), Error>>::new());
        let init_done_clone = init_done.clone();
        thread::spawn(move || {
            let notifying_publisher = match NotifyingPublisher::<ScrapedRelayBlockBidRPC>::new(
                SCRAPED_BIDS_SERVICE_NAME,
                GOT_SCRAPED_BIDS_OR_BLOCKS_EVENT_NAME,
                SCRAPED_BIDS_MAX_BUFFERS,
                SCRAPED_MAX_LOAN_SAMPLES,
                SCRAPED_BIDS_SERVICE_MAX_PUBLISHERS,
                SCRAPED_BIDS_SERVICE_MAX_SUBSCRIBERS,
            ) {
                Ok(notifying_publisher) => {
                    init_done.set(Ok(()));
                    notifying_publisher
                }
                Err(err) => {
                    init_done.set(Err(err));
                    return;
                }
            };
            while let Ok(scraped_bid) = scraped_bids_rx.recv() {
                if let Err(err) = notifying_publisher.send(scraped_bid) {
                    error!(err=?err, "ScrapedBidsPublisher notifying_publisher.send failed. Bid lost.");
                }
            }
        });
        match init_done_clone.wait_for_ever() {
            Ok(_) => Ok(Self {
                scraped_bids_sender,
            }),
            Err(err) => Err(err),
        }
    }

    pub fn send(&self, scraped_bid: ScrapedRelayBlockBidWithStats) {
        if let Err(err) = self
            .scraped_bids_sender
            .send(ScrapedRelayBlockBidRPC::from(scraped_bid))
        {
            error!(err=?err, "scraped_bids_sender.send failed. Bid lost.");
        }
    }
}

/// struct to publish ItemType to the bidding service. RPC is configured to keep only the last item.
/// Adds an extra thread so we can call publisher code from a single thread since it's not Send.
#[derive(Debug)]
pub struct LastItemPublisher<ItemType> {
    last_item: Arc<Watch<ItemType>>,
}

impl<ItemType: Send + Sync + 'static> LastItemPublisher<ItemType> {
    pub fn new<ItemTypeRPC: std::fmt::Debug + ZeroCopySend + 'static>(
        item_service_name: &'static str,
        got_item_event_name: &'static str,
        max_publishers: usize,
        max_subscribers: usize,
        item_to_rpc: impl Fn(ItemType) -> Option<ItemTypeRPC> + Send + Sync + 'static,
        cancellation_token: CancellationToken,
    ) -> Result<Self, Error> {
        let last_item: Arc<Watch<ItemType>> = Arc::new(Watch::new());
        let last_item_clone = last_item.clone();
        let init_done = Arc::new(Watch::<Result<(), Error>>::new());
        let init_done_clone = init_done.clone();
        thread::spawn(move || {
            info!(item_service_name, "Publisher starting");
            let notifying_publisher = match NotifyingPublisher::<ItemTypeRPC>::new(
                item_service_name,
                got_item_event_name,
                LAST_ITEM_MAX_BUFFERS,
                LAST_ITEM_MAX_LOAN_SAMPLES,
                max_publishers,
                max_subscribers,
            ) {
                Ok(notifying_publisher) => {
                    init_done.set(Ok(()));
                    notifying_publisher
                }
                Err(err) => {
                    init_done.set(Err(err));
                    return;
                }
            };
            while !cancellation_token.is_cancelled() {
                if let Some(item) = last_item.wait_for_data() {
                    if let Some(item_rpc) = item_to_rpc(item) {
                        if let Err(err) = notifying_publisher.send(item_rpc) {
                            error!(item_service_name,err=?err, "LastItemPublisher notifying_publisher.send failed. Bid lost.");
                        }
                    } else {
                        error!(
                            item_service_name,
                            "LastItemPublisher item_to_rpc returned None. Item lost."
                        );
                    }
                }
            }
            info!(item_service_name, "Publisher shutting down");
        });
        match init_done_clone.wait_for_ever() {
            Ok(_) => Ok(Self {
                last_item: last_item_clone,
            }),
            Err(err) => Err(err),
        }
    }

    /// Same as new when ItemTypeRPC is From<ItemType>.
    pub fn new_with_from<ItemTypeRPC: std::fmt::Debug + ZeroCopySend + From<ItemType> + 'static>(
        item_service_name: &'static str,
        got_item_event_name: &'static str,
        max_publishers: usize,
        max_subscribers: usize,
        cancellation_token: CancellationToken,
    ) -> Result<Self, Error> {
        Self::new(
            item_service_name,
            got_item_event_name,
            max_publishers,
            max_subscribers,
            |item| Some(ItemTypeRPC::from(item)),
            cancellation_token,
        )
    }

    pub fn send(&self, item: ItemType) {
        self.last_item.set(item);
    }
}

pub type BlocksPublisher = LastItemPublisher<BuiltBlockDescriptorForSlotBidderWithSessionId>;
pub fn create_blocks_publisher(
    cancellation_token: CancellationToken,
) -> Result<BlocksPublisher, Error> {
    BlocksPublisher::new_with_from::<BuiltBlockDescriptorForSlotBidderRPC>(
        BLOCKS_SERVICE_NAME,
        GOT_SCRAPED_BIDS_OR_BLOCKS_EVENT_NAME,
        BLOCKS_SERVICE_MAX_PUBLISHERS,
        BLOCKS_SERVICE_MAX_SUBSCRIBERS,
        cancellation_token,
    )
}

pub type SlotBidderSealBidCommandPublisher =
    LastItemPublisher<SlotBidderSealBidCommandWithSessionId>;
pub fn create_slot_bidder_seal_bid_command_publisher(
    relay_sets: &[RelaySet],
    cancellation_token: CancellationToken,
) -> Result<SlotBidderSealBidCommandPublisher, Error> {
    let relay_sets = relay_sets.to_vec();
    SlotBidderSealBidCommandPublisher::new::<SlotBidderSealBidCommandRPC>(
        SLOT_BIDDER_SEAL_BID_COMMAND_SERVICE_NAME,
        GOT_SLOT_BIDDER_SEAL_BID_COMMAND_EVENT_NAME,
        SLOT_BIDDER_SEAL_BID_COMMAND_SERVICE_MAX_PUBLISHERS,
        SLOT_BIDDER_SEAL_BID_COMMAND_SERVICE_MAX_SUBSCRIBERS,
        move |item| SlotBidderSealBidCommandRPC::try_from(item, &relay_sets),
        cancellation_token,
    )
}

fn init_slot_bidder_seal_bid_command_subscriber() -> Result<
    (
        Listener<ipc::Service>,
        SubscriberPoller<SlotBidderSealBidCommandRPC>,
    ),
    Error,
> {
    let node = create_node_builder()?;
    let slot_bidder_seal_bid_command_service = create_slot_bidder_seal_bid_command_service(&node)?;
    let slot_bidder_seal_bid_command_subscriber = SubscriberPoller::new(
        slot_bidder_seal_bid_command_service,
        LAST_ITEM_MAX_BUFFERS,
        "slot_bidder_seal_bid_command",
    )?;
    let got_slot_bidder_seal_bid_command_event =
        create_got_slot_bidder_seal_bid_command_event_service(&node)?;
    let listener = got_slot_bidder_seal_bid_command_event
        .listener_builder()
        .create()?;
    Ok((listener, slot_bidder_seal_bid_command_subscriber))
}

/// Spawns a thread that subscribes to the SlotBidderSealBidCommandRPC and forwards them to registered BlockSealInterfaceForSlotBidder in session_id_to_slot_bidder.
/// Result tells if the init stage was successful and the thread was able to start polling.
pub fn spawn_slot_bidder_seal_bid_command_subscriber(
    session_id_to_slot_bidder: Arc<
        Mutex<HashMap<u64, Arc<dyn BlockSealInterfaceForSlotBidder + Send + Sync>>>,
    >,
    relay_sets: Vec<RelaySet>,
    cancellation_token: CancellationToken,
) -> Result<(), Error> {
    let init_done = Arc::new(Watch::<Result<(), Error>>::new());
    let init_done_clone = init_done.clone();
    thread::spawn(move || {
        info!("SlotBidderSealBidCommandRPC subscriber thread starting");
        let (listener, mut slot_bidder_seal_bid_command_subscriber) =
            match init_slot_bidder_seal_bid_command_subscriber() {
                Ok((listener, slot_bidder_seal_bid_command_subscriber)) => {
                    init_done.set(Ok(()));
                    (listener, slot_bidder_seal_bid_command_subscriber)
                }
                Err(err) => {
                    init_done.set(Err(err));
                    return;
                }
            };
        while !cancellation_token.is_cancelled() {
            if let Ok(Some(_event_id)) = listener.timed_wait_one(THREAD_BLOCKING_DURATION) {
                if let Err(err) = slot_bidder_seal_bid_command_subscriber.poll(|sample| {
                    let bidder = session_id_to_slot_bidder
                        .lock()
                        .get_mut(&sample.session_id)
                        .cloned();
                    if let Some(bidder) = bidder {
                        if let Some(sample) = SlotBidderSealBidCommandRPC::into_slot_bidder_seal_bid_command(&sample, &relay_sets) {
                            bidder.seal_bid(sample);
                        } else {
                            error!("got seal bid command but could not convert to SlotBidderSealBidCommand");
                        }
                    } else {
                        warn!("got seal bid command but no bidder found",);
                    }
                }) {
                    error!(err=?err, "SlotBidderSealBidCommandRPC subscriber thread poll failed.");
                }
            }
        }
        info!("SlotBidderSealBidCommandRPC subscriber thread shutting down");
    });
    init_done_clone.wait_for_ever()
}
