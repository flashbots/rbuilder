use std::sync::Arc;

use super::interfaces::{BidValueObs, BidValueSource};

/// BidValueSource that will NOT report anything.
#[derive(Debug)]
pub struct NullBidValueSource {}

impl BidValueSource for NullBidValueSource {
    fn subscribe(&self, _block_number: u64, _slot_number: u64, _obs: Arc<dyn BidValueObs>) {}
    fn unsubscribe(&self, _obs: Arc<dyn BidValueObs>) {}
}
