use std::sync::Arc;

use crate::{
    building::builders::{block_building_helper::BlockBuildingHelper, UnfinishedBlockBuildingSink},
    live_builder::block_output::bid_value_source::interfaces::BidValueObs,
};
use alloy_primitives::U256;
use mockall::automock;
use reth_primitives::BlockNumber;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

/// Trait in charge of bidding blocks.
/// It is created for each block / slot.
/// Via UnfinishedBlockBuildingSink it gets the new biddable blocks.
/// Via BidValueObs it gets the competition bids that it should improve when possible.
/// On creation the concrete SlotBidder will get a BidMaker to make the bids.
pub trait SlotBidder: UnfinishedBlockBuildingSink + BidValueObs {}

/// Bid we want to make.
pub struct Bid {
    /// Block we should seal with payout tx of payout_tx_value.
    block: Box<dyn BlockBuildingHelper>,
    /// payout_tx_value should be Some <=> block.can_add_payout_tx()
    payout_tx_value: Option<U256>,
}

impl std::fmt::Debug for Bid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bid")
            .field("payout_tx_value", &self.payout_tx_value)
            .finish_non_exhaustive()
    }
}

impl Bid {
    /// Creates a new Bid instance.
    pub fn new(block: Box<dyn BlockBuildingHelper>, payout_tx_value: Option<U256>) -> Self {
        Self {
            block,
            payout_tx_value,
        }
    }

    pub fn block(self) -> Box<dyn BlockBuildingHelper> {
        self.block
    }

    /// Returns the payout transaction value.
    pub fn payout_tx_value(&self) -> Option<U256> {
        self.payout_tx_value
    }
}

/// Makes the actual bid (seal + send it to the relay).
pub trait BidMaker: std::fmt::Debug {
    fn send_bid(&self, bid: Bid);
}

/// Info about a onchain block from reth.
#[derive(Eq, PartialEq, Clone, Debug)]
pub struct LandedBlockInfo {
    pub block_number: BlockNumber,
    pub block_timestamp: OffsetDateTime,
    pub builder_balance: U256,
    /// true -> we landed this block.
    /// If false we could have landed it in coinbase == fee recipient mode but balance wouldn't change so we don't care.
    pub beneficiary_is_builder: bool,
}

/// Trait in charge of bidding.
/// We use one for the whole execution and ask for a [SlotBidder] for each particular slot.
/// After BiddingService creation the builder will try to feed it all the needed update_new_landed_block_detected from the DB history.
/// To avoid exposing how much info the BiddingService uses we don't ask it anything and feed it the max history we are willing to read.
/// After that the builder will update each block via update_new_landed_block_detected.
pub trait BiddingService: std::fmt::Debug + Send + Sync {
    fn create_slot_bidder(
        &mut self,
        block: u64,
        slot: u64,
        slot_timestamp: OffsetDateTime,
        bid_maker: Box<dyn BidMaker + Send + Sync>,
        cancel: CancellationToken,
    ) -> Arc<dyn SlotBidder>;

    /// Access to BiddingServiceWinControl::must_win_block.
    fn win_control(&self) -> Arc<dyn BiddingServiceWinControl>;

    /// We are notified about some landed blocks.
    /// They are sorted in ascending order.
    /// Consecutive calls will have consecutive block numbers.
    fn update_new_landed_blocks_detected(&mut self, landed_blocks: &[LandedBlockInfo]);

    /// We let the BiddingService know we had some problem reading landed blocks just in case we wants to change his strategy (eg: stop bidding until next update_new_landed_blocks_detected)
    fn update_failed_reading_new_landed_blocks(&mut self);
}

/// Trait to control the must_win_block feature of the BiddingService.
/// It allows to use BiddingService as a Box (single threaded mutable access) but be able to call must_win_block from another thread.
#[automock]
pub trait BiddingServiceWinControl: Send + Sync + std::fmt::Debug {
    /// If called, any current or future SlotBidder working on that block will bid more aggressively to win the block.
    fn must_win_block(&self, block: u64);
}
