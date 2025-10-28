use std::sync::Arc;

use alloy_primitives::U256;
use tokio_util::sync::CancellationToken;

use super::bidding_service_interface::*;

/// Bidding service that bids the true block value + subsidy to all relays.
pub struct NewTrueBlockValueBiddingService {
    subsidy: U256,
    slot_delta_to_start_bidding: time::Duration,
    all_relays: RelaySet,
    /// relay_sets = [all_relays]
    relay_sets: Vec<RelaySet>,
}

impl NewTrueBlockValueBiddingService {
    pub fn new(
        subsidy: U256,
        slot_delta_to_start_bidding: time::Duration,
        all_relays: RelaySet,
    ) -> Self {
        Self {
            subsidy,
            slot_delta_to_start_bidding,
            all_relays: all_relays.clone(),
            relay_sets: vec![all_relays],
        }
    }
}

pub struct NewTrueBlockValueSlotBidder {
    subsidy: U256,
    bid_start_time: time::OffsetDateTime,
    block_seal_handle: Box<dyn BlockSealInterfaceForSlotBidder + Send + Sync>,
    all_relays: RelaySet,
}

impl SlotBidder for NewTrueBlockValueSlotBidder {
    fn notify_new_built_block(&self, block_descriptor: BuiltBlockDescriptorForSlotBidder) {
        if time::OffsetDateTime::now_utc() < self.bid_start_time {
            return;
        }
        self.block_seal_handle.seal_bid(SlotBidderSealBidCommand {
            block_id: block_descriptor.id,
            seen_competition_bid: None,
            trigger_creation_time: Some(time::OffsetDateTime::now_utc()),
            payout_info: vec![PayoutInfo {
                relays: self.all_relays.clone(),
                payout_tx_value: block_descriptor.true_block_value + self.subsidy,
                subsidy: self.subsidy.try_into().unwrap(),
            }],
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
        let bid_start_time = slot_timestamp + self.slot_delta_to_start_bidding;
        Arc::new(NewTrueBlockValueSlotBidder {
            subsidy: self.subsidy,
            bid_start_time,
            block_seal_handle,
            all_relays: self.all_relays.clone(),
        })
    }

    fn relay_sets(&self) -> Vec<RelaySet> {
        self.relay_sets.clone()
    }

    fn observe_relay_bids(&self, _bid: ScrapedRelayBlockBidWithStats) {}

    fn update_new_landed_blocks_detected(&self, _landed_blocks: &[LandedBlockInfo]) {}

    fn update_failed_reading_new_landed_blocks(&self) {}
}
