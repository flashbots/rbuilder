use std::sync::Arc;

use super::bidding_service_client_adapter::BiddingServiceClientCommand;
use crate::bidding_service_wrapper::{
    fast_streams::helpers::BlocksPublisher, DestroySlotBidderParams,
};
use rbuilder::live_builder::block_output::bidding_service_interface::{
    BuiltBlockDescriptorForSlotBidder, SlotBidder,
};
use tokio::sync::mpsc;

/// Implementation of SlotBidder.
/// blocks are published via a BlocksPublisher.
/// On drop sends DestroySlotBidder to the bidding service.
#[derive(Debug)]
pub struct UnfinishedBlockBuildingSinkClient {
    session_id: u64,
    commands_sender: mpsc::UnboundedSender<BiddingServiceClientCommand>,
    blocks_publisher: Arc<BlocksPublisher>,
}

impl UnfinishedBlockBuildingSinkClient {
    pub fn new(
        session_id: u64,
        commands_sender: mpsc::UnboundedSender<BiddingServiceClientCommand>,
        blocks_publisher: Arc<BlocksPublisher>,
    ) -> Self {
        UnfinishedBlockBuildingSinkClient {
            blocks_publisher,
            commands_sender,
            session_id,
        }
    }
}

impl SlotBidder for UnfinishedBlockBuildingSinkClient {
    fn notify_new_built_block(&self, block_descriptor: BuiltBlockDescriptorForSlotBidder) {
        self.blocks_publisher
            .send((block_descriptor, self.session_id));
    }
}

impl Drop for UnfinishedBlockBuildingSinkClient {
    fn drop(&mut self) {
        let _ = self
            .commands_sender
            .send(BiddingServiceClientCommand::DestroySlotBidder(
                DestroySlotBidderParams {
                    session_id: self.session_id,
                },
            ));
    }
}
