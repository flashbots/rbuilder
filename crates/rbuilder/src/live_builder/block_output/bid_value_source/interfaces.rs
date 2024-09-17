use alloy_primitives::U256;
use std::sync::Arc;

pub trait BidValueObs: std::fmt::Debug {
    /// @Pending: add source of the bid.
    fn update_new_bid(&self, bid: U256);
}

/// Object watching a stream af the bids made.
/// Allows us to subscribe to notifications for particular blocks/slots.
pub trait BidValueSource: std::fmt::Debug {
    fn subscribe(&self, block_number: u64, slot_number: u64, obs: Arc<dyn BidValueObs>);
    fn unsubscribe(&self, obs: Arc<dyn BidValueObs>);
}
