use std::sync::Arc;

use alloy_primitives::U256;
use tokio_util::sync::CancellationToken;

use super::bidding_service_interface::*;

pub struct NewTrueBlockValueBiddingService {
    pub subsidy: U256,
    pub slot_delta_to_start_bidding: time::Duration,
}

pub struct NewTrueBlockValueSlotBidder {
    subsidy: U256,
    bid_start_time: time::OffsetDateTime,
    block_seal_handle: Box<dyn BlockSealInterfaceForSlotBidder + Send + Sync>,
}

impl SlotBidder for NewTrueBlockValueSlotBidder {
    fn notify_new_built_block(&self, block_descriptor: BuiltBlockDescriptorForSlotBidder) {
        if time::OffsetDateTime::now_utc() < self.bid_start_time {
            return;
        }
        self.block_seal_handle.seal_bid(SlotBidderSealBidCommand {
            block_id: block_descriptor.id,
            payout_tx_value: if block_descriptor.can_add_payout_tx {
                Some(block_descriptor.true_block_value + self.subsidy)
            } else {
                None
            },
            seen_competition_bid: None,
            trigger_creation_time: Some(time::OffsetDateTime::now_utc()),
        })
    }
}

impl BiddingService for NewTrueBlockValueBiddingService {
    fn create_slot_bidder(
        &self,
        _slot_block_id: SlotBlockId,
        slot_timestamp: time::OffsetDateTime,
        block_seal_handle: Box<dyn BlockSealInterfaceForSlotBidder + Send + Sync>,
        _cancel: CancellationToken,
    ) -> Arc<dyn SlotBidder> {
        block_seal_handle.set_can_use_suggested_fee_recipient_as_coinbase(true);

        let bid_start_time = slot_timestamp + self.slot_delta_to_start_bidding;
        Arc::new(NewTrueBlockValueSlotBidder {
            subsidy: self.subsidy,
            bid_start_time,
            block_seal_handle,
        })
    }

    fn observe_relay_bids(&self, _bid: ScrapedRelayBlockBidWithStats) {}

    fn update_new_landed_blocks_detected(&self, _landed_blocks: &[LandedBlockInfo]) {}

    fn update_failed_reading_new_landed_blocks(&self) {}
}
