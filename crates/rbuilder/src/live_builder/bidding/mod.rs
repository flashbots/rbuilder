use std::sync::Arc;

use alloy_primitives::U256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealInstruction {
    /// Don't waste cycles sealing block that has no chances
    Skip,
    /// Set this value in the last tx before sealing block
    Value(U256),
}

/// Slot bidder is used by builder to decide what value should be put into the last tx.
/// It is created for each block / slot.
pub trait SlotBidder: Send + Sync + std::fmt::Debug {
    /// Returns true if payment for the slot can go directly to fee recipient through coinbase.
    fn is_pay_to_coinbase_allowed(&self) -> bool;

    /// Returns what value needs to be sent to the fee recipient or if block should be skipped.
    fn seal_instruction(&self, unsealed_block_profit: U256) -> SealInstruction;

    /// Returns best bid value available on the relays.
    fn best_bid_value(&self) -> Option<U256>;
}

impl SlotBidder for () {
    fn is_pay_to_coinbase_allowed(&self) -> bool {
        true
    }

    fn seal_instruction(&self, unsealed_block_profit: U256) -> SealInstruction {
        SealInstruction::Value(unsealed_block_profit)
    }

    fn best_bid_value(&self) -> Option<U256> {
        None
    }
}

pub trait BiddingService: std::fmt::Debug {
    fn create_slot_bidder(
        &mut self,
        block: u64,
        slot: u64,
        slot_end_timestamp: u64,
    ) -> Arc<dyn SlotBidder>;
}

/// Creates () which implements the dummy SlotBidder which bids all true value
#[derive(Debug)]
pub struct DummyBiddingService {}
impl BiddingService for DummyBiddingService {
    fn create_slot_bidder(&mut self, _: u64, _: u64, _: u64) -> Arc<dyn SlotBidder> {
        Arc::new(())
    }
}

/// Wrapper around other bidder.
/// If the inner bidder seal_instruction returns SealInstruction::Skip it will change it to pay all profit (SealInstruction::Value(unsealed_block_profit)
#[derive(Debug)]
pub struct AlwaysSealSlotBidderWrapper<SlotBidderType> {
    bidder: SlotBidderType,
}

impl<SlotBidderType: SlotBidder> AlwaysSealSlotBidderWrapper<SlotBidderType> {
    pub fn new(bidder: SlotBidderType) -> Self {
        Self { bidder }
    }
}

impl<SlotBidderType: SlotBidder> SlotBidder for AlwaysSealSlotBidderWrapper<SlotBidderType> {
    fn is_pay_to_coinbase_allowed(&self) -> bool {
        self.bidder.is_pay_to_coinbase_allowed()
    }

    fn seal_instruction(&self, unsealed_block_profit: U256) -> SealInstruction {
        match self.bidder.seal_instruction(unsealed_block_profit) {
            SealInstruction::Skip => SealInstruction::Value(unsealed_block_profit),
            SealInstruction::Value(v) => SealInstruction::Value(v),
        }
    }

    fn best_bid_value(&self) -> Option<U256> {
        self.bidder.best_bid_value()
    }
}
