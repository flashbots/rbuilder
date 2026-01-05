use std::sync::Arc;

use rbuilder::live_builder::block_output::bidding_service_interface::{
    BiddingService, BlockSealInterfaceForSlotBidder, BuiltBlockDescriptorForSlotBidder, SlotBidder,
    SlotBidderSealBidCommand, SlotBlockId,
};
use tokio_util::sync::CancellationToken;
use tonic::Status;

use crate::bidding_service_wrapper::{
    conversion::rpc2real_block_hash,
    fast_streams::helpers::{
        create_slot_bidder_seal_bid_command_publisher, SlotBidderSealBidCommandPublisher,
    },
    CreateSlotBidderParams,
};

/// Handler for calls for a specific SlotBidder.
/// To handle bids generated from the SlotBidder we create a SlotBidderSealBidCommandPublisher that streams the bids via iceoryx.
/// The SlotBidder gets as [BlockSealInterfaceForSlotBidder] a [BlockSealInterfaceForSlotBidderToPublisher] that forwards the bids to the publisher.
pub struct SlotBidderServer {
    bidder: Arc<dyn SlotBidder>,
    cancel: CancellationToken,
}

#[derive(Debug)]
struct BlockSealInterfaceForSlotBidderToPublisher {
    session_id: u64,
    publisher: SlotBidderSealBidCommandPublisher,
}

impl BlockSealInterfaceForSlotBidder for BlockSealInterfaceForSlotBidderToPublisher {
    fn seal_bid(&self, bid: SlotBidderSealBidCommand) {
        self.publisher.send((bid, self.session_id));
    }
}

impl SlotBidderServer {
    #[allow(clippy::result_large_err)]
    pub fn new(
        bidding_service: &mut dyn BiddingService,
        params: &CreateSlotBidderParams,
    ) -> Result<Self, Status> {
        let slot_timestamp = time::OffsetDateTime::from_unix_timestamp(params.slot_timestamp)
            .map_err(|_| Status::invalid_argument("slot_timestamp"))?;
        let parent_block_hash = rpc2real_block_hash(&params.parent_hash)
            .map_err(|_| Status::invalid_argument("parent_block_hash"))?;
        let cancel = CancellationToken::new();
        let publisher = match create_slot_bidder_seal_bid_command_publisher(
            &bidding_service.relay_sets(),
            cancel.clone(),
        ) {
            Ok(publisher) => publisher,
            Err(err) => {
                return Err(Status::internal(err.to_string()));
            }
        };
        let bid_maker = Box::new(BlockSealInterfaceForSlotBidderToPublisher {
            session_id: params.session_id,
            publisher,
        });
        let bidder = bidding_service.create_slot_bidder(
            SlotBlockId::new(params.slot, params.block, parent_block_hash),
            slot_timestamp,
            bid_maker,
            cancel.clone(),
        );
        let res = Self { bidder, cancel };
        Ok(res)
    }

    pub fn new_block(&self, block_descriptor: BuiltBlockDescriptorForSlotBidder) {
        self.bidder.notify_new_built_block(block_descriptor);
    }
}

impl Drop for SlotBidderServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
