use super::interfaces::{BidValueObs, BidValueSource};
use alloy_primitives::U256;
use std::sync::{Arc, Mutex};

/// Simple struct tracking the last best bid and asking it in a sync way via best_bid_value.
pub struct BestBidSyncSource {
    best_bid_source_inner: Arc<BestBidSyncSourceInner>,
    bid_value_source: Arc<dyn BidValueSource + Send + Sync>,
}

impl Drop for BestBidSyncSource {
    fn drop(&mut self) {
        self.bid_value_source
            .unsubscribe(self.best_bid_source_inner.clone());
    }
}

impl BestBidSyncSource {
    pub fn new(
        bid_value_source: Arc<dyn BidValueSource + Send + Sync>,
        block_number: u64,
        slot_number: u64,
    ) -> Self {
        let best_bid_source_inner = Arc::new(BestBidSyncSourceInner::default());
        bid_value_source.subscribe(block_number, slot_number, best_bid_source_inner.clone());
        Self {
            best_bid_source_inner,
            bid_value_source,
        }
    }

    pub fn best_bid_value(&self) -> Option<U256> {
        *self.best_bid_source_inner.best_bid.lock().unwrap()
    }
}

#[derive(Debug, Default)]
struct BestBidSyncSourceInner {
    best_bid: Mutex<Option<U256>>,
}

impl BidValueObs for BestBidSyncSourceInner {
    fn update_new_bid(&self, bid: U256) {
        let mut best_bid = self.best_bid.lock().unwrap();
        if best_bid.map_or(true, |old_bid| old_bid < bid) {
            *best_bid = Some(bid);
        }
    }
}
