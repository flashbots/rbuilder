use alloy_primitives::U256;
use mockall::automock;
use std::sync::Arc;
use time::OffsetDateTime;

#[derive(Clone, Debug)]
pub struct CompetitionBid {
    bid: U256,
    /// For metrics. Set on creation which is the first time we see it in our process.
    creation_time: OffsetDateTime,
}

impl CompetitionBid {
    pub fn new(bid: U256) -> Self {
        Self {
            bid,
            creation_time: OffsetDateTime::now_utc(),
        }
    }

    pub fn new_for_deserialization(bid: U256, creation_time: OffsetDateTime) -> Self {
        Self { bid, creation_time }
    }

    pub fn bid(&self) -> U256 {
        self.bid
    }

    pub fn creation_time(&self) -> OffsetDateTime {
        self.creation_time
    }
}

/// Sync + Send to allow to be called from another thread.
#[automock]
pub trait BidValueObs: std::fmt::Debug + Sync + Send {
    /// @Pending: add source of the bid.
    fn update_new_bid(&self, bid: CompetitionBid);
}

/// Object watching a stream af the bids made.
/// Allows us to subscribe to notifications for particular blocks/slots.
pub trait BidValueSource: std::fmt::Debug {
    fn subscribe(&self, block_number: u64, slot_number: u64, obs: Arc<dyn BidValueObs>);
    fn unsubscribe(&self, obs: Arc<dyn BidValueObs>);
}
