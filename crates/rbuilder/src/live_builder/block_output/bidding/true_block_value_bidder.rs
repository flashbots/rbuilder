use super::interfaces::{
    Bid, BidMaker, BiddingService, BiddingServiceWinControl, LandedBlockInfo, SlotBidder,
};
use crate::{
    building::builders::{
        block_building_helper::BiddableUnfinishedBlock, UnfinishedBlockBuildingSink,
    },
    live_builder::block_output::bid_value_source::interfaces::{BidValueObs, CompetitionBid},
};
use alloy_primitives::U256;
use parking_lot::Mutex;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// Bidding service giving a TrueBlockValueBidder.
/// This is just an example not really suitable for production since it gives away all the profit!.
#[derive(Debug)]
pub struct TrueBlockValueBiddingService {
    slot_delta_to_start_bidding: time::Duration,
    subsidy: U256,
}

impl TrueBlockValueBiddingService {
    /// _landed_blocks is passed to look like a real BiddingService...
    pub fn new(
        _landed_blocks: &[LandedBlockInfo],
        slot_delta_to_start_bidding: time::Duration,
        subsidy: U256,
    ) -> Self {
        Self {
            slot_delta_to_start_bidding,
            subsidy,
        }
    }
}

impl BiddingService for TrueBlockValueBiddingService {
    fn create_slot_bidder(
        &mut self,
        _block: u64,
        _slot: u64,
        slot_timestamp: OffsetDateTime,
        bid_maker: Box<dyn BidMaker + Send + Sync>,
        cancel: CancellationToken,
    ) -> Arc<dyn SlotBidder> {
        let bid_start = slot_timestamp + self.slot_delta_to_start_bidding;
        let delay = core::time::Duration::try_from(bid_start - OffsetDateTime::now_utc());
        let inner = if let Ok(delay) = delay {
            // We create a task that sleeps, sends the last blocks and then "gives back" the bidder.
            let inner = Arc::new(Mutex::new(TrueBlockValueBidderInner {
                bid_maker: None,
                last_block_not_sent: None,
                subsidy: self.subsidy,
            }));
            let inner_clone = inner.clone();
            tokio::task::spawn(async move {
                tokio::select! {
                    _ = sleep(delay) => {},
                    _ = cancel.cancelled() => return
                }
                let mut inner = inner_clone.lock();
                if let Some(block) = inner.last_block_not_sent.take() {
                    send_block(block, inner.subsidy, bid_maker.as_ref());
                }
                inner.bid_maker = Some(bid_maker);
            });
            inner
        } else {
            Arc::new(Mutex::new(TrueBlockValueBidderInner {
                bid_maker: Some(bid_maker),
                last_block_not_sent: None,
                subsidy: self.subsidy,
            }))
        };

        Arc::new(TrueBlockValueBidder { inner })
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

/// This struct coordinates the wait time for the first bid.
struct TrueBlockValueBidderInner {
    /// If None UnfinishedBlockBuildingSink::new_block won't bid and instead will store in last_block_not_sent.
    bid_maker: Option<Box<dyn BidMaker + Send + Sync>>,
    last_block_not_sent: Option<BiddableUnfinishedBlock>,
    subsidy: U256,
}

impl std::fmt::Debug for TrueBlockValueBidderInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrueBlockValueBidderInner").finish()
    }
}

/// Bidder that bids every block using its true block value ignoring competition bids.
#[derive(Debug)]
struct TrueBlockValueBidder {
    inner: Arc<Mutex<TrueBlockValueBidderInner>>,
}

impl SlotBidder for TrueBlockValueBidder {}

fn send_block(
    block: BiddableUnfinishedBlock,
    subsidy: U256,
    bid_maker: &(dyn BidMaker + Send + Sync),
) {
    let payout_tx_value = if block.can_add_payout_tx() {
        Some(block.true_block_value() + subsidy)
    } else {
        None
    };
    bid_maker.send_bid(Bid::new(block, payout_tx_value, None));
}

impl UnfinishedBlockBuildingSink for TrueBlockValueBidder {
    fn new_block(&self, block: BiddableUnfinishedBlock) {
        let mut inner = self.inner.lock();
        if let Some(bid_maker) = &inner.bid_maker {
            send_block(block, inner.subsidy, bid_maker.as_ref());
        } else {
            inner.last_block_not_sent = Some(block);
        }
    }

    fn can_use_suggested_fee_recipient_as_coinbase(&self) -> bool {
        false
    }
}

impl BidValueObs for TrueBlockValueBidder {
    fn update_new_bid(&self, _bid: CompetitionBid) {}
}

#[derive(Debug)]
struct TrueBlockValueBiddingServiceWinControl {}

impl BiddingServiceWinControl for TrueBlockValueBiddingServiceWinControl {
    fn must_win_block(&self, _block: u64) {}
}
