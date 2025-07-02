use std::sync::Arc;

use crate::{
    building::builders::{
        block_building_helper::{BiddableUnfinishedBlock, BlockBuildingHelper},
        UnfinishedBlockBuildingSink,
    },
    live_builder::block_output::bidding::block_bid_with_stats::BlockBidWithStats,
};
use alloy_primitives::{BlockHash, BlockNumber, U256};
use mockall::automock;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

/// Bid we want to make.
pub struct Bid {
    /// Block we should seal with payout tx of payout_tx_value.
    block: BiddableUnfinishedBlock,
    /// payout_tx_value should be Some <=> block.can_add_payout_tx()
    payout_tx_value: Option<U256>,
    /// Value we saw in the competition when we decided to make this bid.
    seen_competition_bid: Option<U256>,
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
    pub fn new(
        block: BiddableUnfinishedBlock,
        payout_tx_value: Option<U256>,
        seen_competition_bid: Option<U256>,
    ) -> Self {
        Self {
            block,
            payout_tx_value,
            seen_competition_bid,
        }
    }

    pub fn block(self) -> Box<dyn BlockBuildingHelper> {
        self.block.into_building_helper()
    }

    pub fn payout_tx_value(&self) -> Option<U256> {
        self.payout_tx_value
    }

    pub fn seen_competition_bid(&self) -> Option<U256> {
        self.seen_competition_bid
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

/// Sink for BlockBidWithStats
pub trait BlockBidWithStatsObs: Send + Sync {
    /// Be careful, we don't assume any kind of filtering here so bid may contain our own bids.
    fn update_new_bid(&self, bid_with_stats: BlockBidWithStats);
}

/// Uniquely identifies the head of the chain we are bidding.
#[derive(Eq, PartialEq, Clone, Debug, Hash)]
pub struct SlotBlockId {
    slot: u64,
    /// Redundant with block_parent_hash... think about removing.
    block: u64,
    parent_block_hash: BlockHash,
}

impl SlotBlockId {
    /// Creates a new SlotBlockId instance.
    pub fn new(slot: u64, block: u64, parent_block_hash: BlockHash) -> Self {
        Self {
            slot,
            block,
            parent_block_hash,
        }
    }

    pub fn slot(&self) -> u64 {
        self.slot
    }

    pub fn block(&self) -> u64 {
        self.block
    }

    pub fn parent_block_hash(&self) -> &BlockHash {
        &self.parent_block_hash
    }
}

/// Trait in charge of bidding.
/// After BiddingService creation the builder will try to feed it all the needed update_new_landed_block_detected from the DB history.
/// To avoid exposing how much info the BiddingService uses we don't ask it anything and feed it the max history we are willing to read.
/// After that the builder will update each block via update_new_landed_block_detected.
/// We use one for the whole execution and ask for a [SlotBidder] for each particular slot.
/// We must feed any bid seen via update_new_seen_bid.
pub trait BiddingService: BlockBidWithStatsObs + std::fmt::Debug + Send + Sync {
    fn create_slot_bidder(
        &self,
        slot_block_id: SlotBlockId,
        slot_timestamp: OffsetDateTime,
        bid_maker: Box<dyn BidMaker + Send + Sync>,
        cancel: CancellationToken,
    ) -> Arc<dyn UnfinishedBlockBuildingSink>;

    /// Access to BiddingServiceWinControl::must_win_block.
    fn win_control(&self) -> Arc<dyn BiddingServiceWinControl>;

    /// We are notified about some landed blocks.
    /// They are sorted in ascending order.
    /// Consecutive calls will have consecutive block numbers.
    fn update_new_landed_blocks_detected(&self, landed_blocks: &[LandedBlockInfo]);

    /// We let the BiddingService know we had some problem reading landed blocks just in case we wants to change his strategy (eg: stop bidding until next update_new_landed_blocks_detected)
    fn update_failed_reading_new_landed_blocks(&self);
}

/// Trait to control the must_win_block feature of the BiddingService.
/// It allows to use BiddingService as a Box (single threaded mutable access) but be able to call must_win_block from another thread.
#[automock]
pub trait BiddingServiceWinControl: Send + Sync + std::fmt::Debug {
    /// If called, any current or future SlotBidder working on that block will bid more aggressively to win the block.
    fn must_win_block(&self, block: u64);
}
