use alloy_primitives::U256;
use reth_primitives::SealedBlock;

use crate::{building::BuiltBlockTrace, live_builder::payload_events::MevBoostSlotData};

/// Trait that receives every bid made by us to the relays.
pub trait BidObserver: std::fmt::Debug {
    /// This callback is executed after the bid was made so it gives away ownership of the data.
    /// This should NOT block since it's executed in the submitting thread.
    fn block_submitted(
        &self,
        slot_data: &MevBoostSlotData,
        built_block_trace: &BuiltBlockTrace,
        builder_name: String,
        best_bid_value: U256,
    );
}

#[derive(Debug)]
pub struct NullBidObserver {}

impl BidObserver for NullBidObserver {
    fn block_submitted(
        &self,
        _slot_data: &MevBoostSlotData,
        _submit_block_request: &SubmitBlockRequest,
        _built_block_trace: &BuiltBlockTrace,
        _builder_name: String,
        _best_bid_value: U256,
    ) {
    }
}
