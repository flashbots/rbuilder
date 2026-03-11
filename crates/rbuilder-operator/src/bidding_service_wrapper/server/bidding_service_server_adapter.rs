use std::{sync::Arc, thread};

use crate::bidding_service_wrapper::{
    conversion::rpc2real_landed_block_info,
    fast_streams::{
        helpers::{
            create_blocks_service, create_got_scraped_bids_or_blocks_service, create_node_builder,
            create_scraped_bids_service, LAST_ITEM_MAX_BUFFERS, SCRAPED_BIDS_MAX_BUFFERS,
        },
        subscriber_poller::SubscriberPoller,
        types::{BuiltBlockDescriptorForSlotBidderRPC, ScrapedRelayBlockBidRPC},
    },
    CreateSlotBidderParams, DestroySlotBidderParams, Empty, LandedBlocksParams, MustWinBlockParams,
};
use iceoryx2::{port::listener::Listener, service::ipc};
use parking_lot::Mutex;
use rbuilder::utils::sync::{Watch, THREAD_BLOCKING_DURATION};

use ahash::HashMap;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::{error, info};

use super::{
    bidding_service_sync::BiddingServiceSync,
    bidding_service_with_reload::BiddingServiceWithReload, slot_bidder_server::SlotBidderServer,
};

pub type SlotBidderId = u64;

/// Server side implementation of bidding_service_server::BiddingService.
/// Non bidding calls go directly to the BiddingService.
/// Creates SlotBidderServer on create_slot_bidder and stores them on BiddingServiceServerInner::bidders and destroys them on destroy_slot_bidder.
/// All bidding calls are handled by the associated SlotBidderServer.
/// A thread is spawned to poll blocks and scraped bids (via iceoryx) from the builder and forwards them to the BiddingServiceServerInner.
/// BiddingServiceServerInner will forward blocks to the associated SlotBidder and scraped bids to the BiddingService.
pub struct BiddingServiceServerAdapter {
    inner: Arc<Mutex<BiddingServiceServerInner>>,
    cancellation_token: CancellationToken,
}

struct BiddingServiceServerInner {
    service: Box<dyn BiddingServiceWithReload>,
    bidders: HashMap<SlotBidderId, SlotBidderServer>,
}

impl BiddingServiceServerInner {
    /// usage if the name of the calling func or the "reason" to get a bidder. Used to log on errors.
    fn bidder(&self, slot_bidder_id: SlotBidderId, usage: &str) -> Option<&SlotBidderServer> {
        if let Some(bidder) = self.bidders.get(&slot_bidder_id) {
            Some(bidder)
        } else {
            error!(slot_bidder_id=?slot_bidder_id,usage,"No bidder found");
            None
        }
    }

    /// Forward to SlotBidderServer
    pub fn new_block(&self, block_descriptor: BuiltBlockDescriptorForSlotBidderRPC) {
        if let Some(bidder) = self.bidder(block_descriptor.session_id, "new_block") {
            bidder.new_block(block_descriptor.into());
        }
    }

    /// Forward to bidding_service
    pub fn update_new_bids(&mut self, bids: Vec<ScrapedRelayBlockBidRPC>) {
        self.service
            .bidding_service()
            .observe_relay_bids(bids.into_iter().map(|b| b.into()).collect());
    }
}

impl BiddingServiceServerAdapter {
    pub fn new(
        service: Box<dyn BiddingServiceWithReload>,
        cancellation_token: CancellationToken,
    ) -> Result<Self, crate::bidding_service_wrapper::fast_streams::helpers::Error> {
        let cancellation_token = cancellation_token.child_token();
        let inner = Arc::new(Mutex::new(BiddingServiceServerInner {
            service,
            bidders: Default::default(),
        }));
        spawn_scraped_bids_and_blocks_subscriber(inner.clone(), cancellation_token.clone())?;
        Ok(Self {
            inner,
            cancellation_token,
        })
    }
}

impl Drop for BiddingServiceServerAdapter {
    fn drop(&mut self) {
        self.cancellation_token.cancel();
    }
}

#[tonic::async_trait]
impl BiddingServiceSync for BiddingServiceServerAdapter {
    /// Creates a SlotBidderServer which will handle all bidding related calls (eg: new block, new competition bid) and will feed our bids into a channel.
    /// Returns the callback stream containing the bids and any other state changes.
    fn create_slot_bidder(
        &self,
        request: Request<CreateSlotBidderParams>,
    ) -> Result<Response<Empty>, Status> {
        let params = request.into_inner();
        let mut inner = self.inner.lock();
        match SlotBidderServer::new(inner.service.as_mut().bidding_service(), &params) {
            Ok(slot_bidder_server) => {
                let slot_bidder_id = params.session_id;
                if inner
                    .bidders
                    .insert(slot_bidder_id, slot_bidder_server)
                    .is_some()
                {
                    error!(slot_bidder_id = ?slot_bidder_id,"create_slot_bidder with already created bidder")
                } else {
                    info!(
                        slot_bidder_id = ?slot_bidder_id,
                        "SlotBidder created!"
                    );
                }
            }
            Err(err) => {
                error!(err=?err,"create_slot_bidder SlotBidderServer::new err");
                return Err(Status::invalid_argument(""));
            }
        }
        Ok(Response::new(Empty {}))
    }

    fn destroy_slot_bidder(
        &self,
        request: Request<DestroySlotBidderParams>,
    ) -> Result<Response<Empty>, Status> {
        let params = request.into_inner();
        let mut inner = self.inner.lock();
        let slot_bidder_id = params.session_id;
        if inner.bidders.remove(&slot_bidder_id).is_none() {
            error!(slot_bidder_id = ?slot_bidder_id,"destroy_slot_bidder failed to remove")
        } else {
            info!(
                slot_bidder_id = ?slot_bidder_id,
                "SlotBidder destroyed"
            );
        }
        Ok(Response::new(Empty {}))
    }

    /// Direct forwarding
    fn must_win_block(
        &self,
        _request: Request<MustWinBlockParams>,
    ) -> Result<Response<Empty>, Status> {
        // this was removed because only NullWinControl was used here before
        Ok(Response::new(Empty {}))
    }

    /// Direct forwarding
    fn update_new_landed_blocks_detected(
        &self,
        request: Request<LandedBlocksParams>,
    ) -> Result<Response<Empty>, Status> {
        let params = request.into_inner();
        let block_info: Result<Vec<_>, _> = params
            .landed_block_info
            .iter()
            .map(rpc2real_landed_block_info)
            .collect();
        let block_info = block_info?;
        self.inner
            .lock()
            .service
            .bidding_service()
            .update_new_landed_blocks_detected(&block_info);
        Ok(Response::new(Empty {}))
    }

    /// Direct forwarding
    fn update_failed_reading_new_landed_blocks(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Empty>, Status> {
        self.inner
            .lock()
            .service
            .bidding_service()
            .update_failed_reading_new_landed_blocks();
        Ok(Response::new(Empty {}))
    }

    fn reload(&self) {
        self.inner.lock().service.reload();
    }
}

#[allow(clippy::type_complexity)]
fn init_scraped_bids_and_blocks_subscriber() -> Result<
    (
        Listener<ipc::Service>,
        SubscriberPoller<ScrapedRelayBlockBidRPC>,
        SubscriberPoller<BuiltBlockDescriptorForSlotBidderRPC>,
    ),
    crate::bidding_service_wrapper::fast_streams::helpers::Error,
> {
    let node = create_node_builder()?;
    let scraped_bids_service = create_scraped_bids_service(&node)?;
    let scraped_bids_subscriber = SubscriberPoller::new(
        scraped_bids_service,
        SCRAPED_BIDS_MAX_BUFFERS,
        "scraped_bids",
    )?;
    let blocks_service = create_blocks_service(&node)?;
    let blocks_subscriber = SubscriberPoller::new(blocks_service, LAST_ITEM_MAX_BUFFERS, "blocks")?;
    let got_scraped_bids_or_blocks = create_got_scraped_bids_or_blocks_service(&node)?;
    let listener = got_scraped_bids_or_blocks.listener_builder().create()?;
    Ok((listener, scraped_bids_subscriber, blocks_subscriber))
}

/// Spawns a thread that subscribes to the scraped bids and new blocks and forwards them to rpc_service.
fn spawn_scraped_bids_and_blocks_subscriber(
    inner: Arc<Mutex<BiddingServiceServerInner>>,
    cancellation_token: CancellationToken,
) -> Result<(), crate::bidding_service_wrapper::fast_streams::helpers::Error> {
    let init_done = Arc::new(Watch::<
        Result<(), crate::bidding_service_wrapper::fast_streams::helpers::Error>,
    >::new());
    let init_done_clone = init_done.clone();
    thread::spawn(move || {
        info!("Subscriber thread starting");
        let (listener, mut scraped_bids_subscriber, mut blocks_subscriber) =
            match init_scraped_bids_and_blocks_subscriber() {
                Ok(res) => res,
                Err(err) => {
                    init_done.set(Err(err));
                    return;
                }
            };
        init_done.set(Ok(()));
        while !cancellation_token.is_cancelled() {
            if let Ok(Some(_event_id)) = listener.timed_wait_one(THREAD_BLOCKING_DURATION) {
                let mut bids = Vec::new();
                if let Err(err) = scraped_bids_subscriber.poll(|sample| {
                    bids.push(sample);
                }) {
                    error!(err=?err, "scraped_bids_subscriber poll failed.");
                }
                if !bids.is_empty() {
                    inner.lock().update_new_bids(bids);
                }
                if let Err(err) = blocks_subscriber.poll(|sample| {
                    inner.lock().new_block(sample);
                }) {
                    error!(err=?err, "blocks_subscriber poll failed.");
                }
            }
        }
        info!("Subscriber thread shutting down");
    });
    init_done_clone.wait_for_ever()
}
