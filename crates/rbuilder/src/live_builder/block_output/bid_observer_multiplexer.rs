use reth_primitives::SealedBlock;

use crate::{building::BuiltBlockTrace, live_builder::payload_events::MevBoostSlotData};

use super::bid_observer::BidObserver;

/// Implements BidObserver forwarding all calls to several BidObservers.
#[derive(Default)]
pub struct BidObserverMultiplexer {
    observers: Vec<Box<dyn BidObserver + Send + Sync>>,
}

impl std::fmt::Debug for BidObserverMultiplexer {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("BidObserverMultiplexer").finish()
    }
}

impl BidObserverMultiplexer {
    pub fn push(&mut self, obs: Box<dyn BidObserver + Send + Sync>) {
        self.observers.push(obs);
    }
}

impl BidObserver for BidObserverMultiplexer {
    fn block_submitted(
        &self,
        slot_data: &MevBoostSlotData,
        submit_block_request: &SubmitBlockRequest,
        built_block_trace: &BuiltBlockTrace,
        builder_name: String,
        best_bid_value: alloy_primitives::U256,
    ) {
        for obs in &self.observers {
            obs.block_submitted(
                slot_data,
                built_block_trace,
                builder_name.clone(),
                best_bid_value,
            );
        }
    }
}
