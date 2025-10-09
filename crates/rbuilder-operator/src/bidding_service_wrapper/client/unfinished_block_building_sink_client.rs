use rbuilder::{
    live_builder::block_output::bidding_service_interface::{
        BuiltBlockDescriptorForSlotBidder, SlotBidder,
    },
    utils::offset_datetime_to_timestamp_us,
};
use tokio::sync::mpsc;

use crate::bidding_service_wrapper::{DestroySlotBidderParams, NewBlockParams};

use super::bidding_service_client_adapter::BiddingServiceClientCommand;

/// Implementation of SlotBidder.
/// Commands are forwarded everything to a UnboundedSender<BiddingServiceClientCommand>.
/// BidMaker is wrapped with ... that contains a poling task that makes the bids.
#[derive(Debug)]
pub struct UnfinishedBlockBuildingSinkClient {
    session_id: u64,
    commands_sender: mpsc::UnboundedSender<BiddingServiceClientCommand>,
}

impl UnfinishedBlockBuildingSinkClient {
    pub fn new(
        session_id: u64,
        commands_sender: mpsc::UnboundedSender<BiddingServiceClientCommand>,
    ) -> Self {
        UnfinishedBlockBuildingSinkClient {
            commands_sender,
            session_id,
        }
    }
}

impl SlotBidder for UnfinishedBlockBuildingSinkClient {
    fn notify_new_built_block(&self, block_descriptor: BuiltBlockDescriptorForSlotBidder) {
        let _ = self
            .commands_sender
            .send(BiddingServiceClientCommand::NewBlock(NewBlockParams {
                session_id: self.session_id,
                true_block_value: block_descriptor.true_block_value.as_limbs().to_vec(),
                can_add_payout_tx: true,
                block_id: block_descriptor.id.0,
                creation_time_us: offset_datetime_to_timestamp_us(block_descriptor.creation_time),
            }));
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
