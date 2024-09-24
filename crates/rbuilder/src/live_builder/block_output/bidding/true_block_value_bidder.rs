use super::interfaces::{
    Bid, BidMaker, BiddingService, BiddingServiceWinControl, LandedBlockInfo, SlotBidder,
};
use crate::{
    building::builders::{block_building_helper::BlockBuildingHelper, UnfinishedBlockBuildingSink},
    live_builder::block_output::bid_value_source::interfaces::BidValueObs,
};
use alloy_primitives::U256;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

/// Bidding service giving a TrueBlockValueBidder
#[derive(Debug)]
pub struct TrueBlockValueBiddingService {}

impl TrueBlockValueBiddingService {
    /// _landed_blocks is passed to look like a real BiddingService...
    pub fn new(_landed_blocks: &[LandedBlockInfo]) -> Self {
        Self {}
    }
}

impl BiddingService for TrueBlockValueBiddingService {
    fn create_slot_bidder(
        &mut self,
        _block: u64,
        _slot: u64,
        _slot_timestamp: OffsetDateTime,
        bid_maker: Box<dyn BidMaker + Send + Sync>,
        _cancel: CancellationToken,
    ) -> Arc<dyn SlotBidder> {
        Arc::new(TrueBlockValueBidder { bid_maker })
    }

    /// Dummy win control.
    fn win_control(&self) -> Arc<dyn BiddingServiceWinControl> {
        Arc::new(TrueBlockValueBiddingServiceWinControl {})
    }

    fn update_new_landed_blocks_detected(&mut self, _landed_blocks: &[LandedBlockInfo]) {
        // No special behavior for landed blocks in this simple implementation.
    }

    fn update_failed_reading_new_landed_blocks(&mut self) {
        // No special behavior for landed blocks in this simple implementation.
    }
}

/// Bidder that bids every block using its true block value ignoring competition bids.
#[derive(Debug)]
struct TrueBlockValueBidder {
    bid_maker: Box<dyn BidMaker + Send + Sync>,
}

impl SlotBidder for TrueBlockValueBidder {}

impl UnfinishedBlockBuildingSink for TrueBlockValueBidder {
    fn new_block(&self, block: Box<dyn BlockBuildingHelper>) {
        let payout_tx_value = if block.can_add_payout_tx() {
            match block.true_block_value() {
                Ok(tbv) => Some(tbv),
                Err(_) => return,
            }
        } else {
            None
        };
        self.bid_maker.send_bid(Bid::new(block, payout_tx_value));
    }

    fn can_use_suggested_fee_recipient_as_coinbase(&self) -> bool {
        true
    }
}

impl BidValueObs for TrueBlockValueBidder {
    fn update_new_bid(&self, _bid: U256) {}
}

#[derive(Debug)]
struct TrueBlockValueBiddingServiceWinControl {}

impl BiddingServiceWinControl for TrueBlockValueBiddingServiceWinControl {
    fn must_win_block(&self, _block: u64) {}
}
