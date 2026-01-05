use std::collections::HashSet;

use alloy_primitives::U256;
use alloy_rpc_types_beacon::BlsPublicKey;
use parking_lot::RwLock;

use super::bidding_service_interface::ScrapedRelayBlockBidWithStats;

/// Counter bid value for non-whitelisted builders (0.0005 ETH in wei)
pub const NON_WHITELISTED_COUNTER_BID: U256 = U256::from_limbs([500_000_000_000_000u64, 0, 0, 0]);

/// Holds the best current bid for a block slot
#[derive(Debug, Clone)]
pub struct BestSlotBid {
    /// The slot this bid is for
    pub slot: u64,
    /// The block number
    pub block: u64,
    /// The best bid value seen
    pub value: U256,
    /// The builder who submitted the best bid
    pub builder_pubkey: Option<BlsPublicKey>,
    /// Full bid details
    pub bid: ScrapedRelayBlockBidWithStats,
}

impl BestSlotBid {
    pub fn new(bid: ScrapedRelayBlockBidWithStats) -> Self {
        Self {
            slot: bid.bid.slot_number,
            block: bid.bid.block_number,
            value: bid.bid.value,
            builder_pubkey: bid.bid.builder_pubkey,
            bid,
        }
    }

    /// Returns true if this bid is better (higher value) than another
    pub fn is_better_than(&self, other: &BestSlotBid) -> bool {
        self.value > other.value
    }
}

/// Evaluates counter bids based on builder whitelist status
pub trait CounterBidEvaluator: Send + Sync {
    /// Evaluate what counter bid to make against the current best bid
    /// Returns the counter bid amount
    fn evaluate_counter_bid(&self, best_bid: &BestSlotBid) -> U256;

    /// Check if a builder is whitelisted
    fn is_whitelisted(&self, builder_pubkey: Option<&BlsPublicKey>) -> bool;
}

/// Counter bid evaluator that returns 0.0005 ETH for non-whitelisted builders
pub struct WhitelistCounterBidEvaluator {
    whitelisted_builders: HashSet<BlsPublicKey>,
}

impl WhitelistCounterBidEvaluator {
    pub fn new(whitelisted_builders: HashSet<BlsPublicKey>) -> Self {
        Self {
            whitelisted_builders,
        }
    }

    pub fn from_vec(builders: Vec<BlsPublicKey>) -> Self {
        Self {
            whitelisted_builders: builders.into_iter().collect(),
        }
    }
}

impl CounterBidEvaluator for WhitelistCounterBidEvaluator {
    fn evaluate_counter_bid(&self, best_bid: &BestSlotBid) -> U256 {
        if self.is_whitelisted(best_bid.builder_pubkey.as_ref()) {
            // Whitelisted builder - no counter bid
            U256::ZERO
        } else {
            // Non-whitelisted builder - counter bid of 0.0005 ETH
            NON_WHITELISTED_COUNTER_BID
        }
    }

    fn is_whitelisted(&self, builder_pubkey: Option<&BlsPublicKey>) -> bool {
        builder_pubkey.is_some_and(|pk| self.whitelisted_builders.contains(pk))
    }
}

/// Thread-safe tracker for the best bid per slot
pub struct BestSlotBidTracker {
    current_best: RwLock<Option<BestSlotBid>>,
    counter_bid_evaluator: Box<dyn CounterBidEvaluator>,
}

impl BestSlotBidTracker {
    pub fn new(counter_bid_evaluator: Box<dyn CounterBidEvaluator>) -> Self {
        Self {
            current_best: RwLock::new(None),
            counter_bid_evaluator,
        }
    }

    /// Update with a new bid, keeping only if it's the best for the current slot
    pub fn update(&self, bid: ScrapedRelayBlockBidWithStats) {
        let new_bid = BestSlotBid::new(bid);
        let mut guard = self.current_best.write();

        match guard.as_ref() {
            Some(existing) => {
                // Only update if same slot and better value, or newer slot
                if new_bid.slot > existing.slot
                    || (new_bid.slot == existing.slot && new_bid.is_better_than(existing))
                {
                    *guard = Some(new_bid);
                }
            }
            None => {
                *guard = Some(new_bid);
            }
        }
    }

    /// Get the current best bid for a specific slot
    pub fn get_best_for_slot(&self, slot: u64) -> Option<BestSlotBid> {
        self.current_best
            .read()
            .as_ref()
            .filter(|b| b.slot == slot)
            .cloned()
    }

    /// Get the current best bid regardless of slot
    pub fn get_current_best(&self) -> Option<BestSlotBid> {
        self.current_best.read().clone()
    }

    /// Evaluate the counter bid for the current best bid
    pub fn evaluate_counter_bid(&self, slot: u64) -> Option<U256> {
        self.get_best_for_slot(slot)
            .map(|best| self.counter_bid_evaluator.evaluate_counter_bid(&best))
    }

    /// Check if the top bidder for a slot is whitelisted
    pub fn is_top_bidder_whitelisted(&self, slot: u64) -> Option<bool> {
        self.get_best_for_slot(slot)
            .map(|best| self.counter_bid_evaluator.is_whitelisted(best.builder_pubkey.as_ref()))
    }

    /// Clear the current best bid (e.g., on slot transition)
    pub fn clear(&self) {
        *self.current_best.write() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use bid_scraper::types::{PublisherType, ScrapedRelayBlockBid};
    use time::OffsetDateTime;

    fn make_pubkey(id: u8) -> BlsPublicKey {
        let mut bytes = [0u8; 48];
        bytes[0] = id;
        bytes.into()
    }

    fn make_bid(slot: u64, value: u64, builder_pubkey: Option<BlsPublicKey>) -> ScrapedRelayBlockBidWithStats {
        ScrapedRelayBlockBidWithStats {
            bid: ScrapedRelayBlockBid {
                seen_time: 0.0,
                publisher_name: "test-publisher".to_string(),
                publisher_type: PublisherType::RelayBids,
                relay_time: None,
                relay_name: "test".to_string(),
                value: U256::from(value),
                slot_number: slot,
                block_number: 100,
                block_hash: B256::ZERO,
                parent_hash: B256::ZERO,
                builder_pubkey,
                extra_data: None,
                fee_recipient: None,
                proposer_fee_recipient: None,
                gas_used: None,
                optimistic_submission: None,
            },
            creation_time: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn test_counter_bid_non_whitelisted() {
        let whitelisted = make_pubkey(1);
        let evaluator = WhitelistCounterBidEvaluator::from_vec(vec![whitelisted]);
        let tracker = BestSlotBidTracker::new(Box::new(evaluator));

        let non_whitelisted = make_pubkey(2);
        tracker.update(make_bid(1, 1_000_000, Some(non_whitelisted)));
        let counter = tracker.evaluate_counter_bid(1).unwrap();
        assert_eq!(counter, NON_WHITELISTED_COUNTER_BID);
    }

    #[test]
    fn test_counter_bid_whitelisted() {
        let whitelisted = make_pubkey(1);
        let evaluator = WhitelistCounterBidEvaluator::from_vec(vec![whitelisted]);
        let tracker = BestSlotBidTracker::new(Box::new(evaluator));

        tracker.update(make_bid(1, 1_000_000, Some(whitelisted)));
        let counter = tracker.evaluate_counter_bid(1).unwrap();
        assert_eq!(counter, U256::ZERO);
    }

    #[test]
    fn test_counter_bid_no_builder_pubkey() {
        let whitelisted = make_pubkey(1);
        let evaluator = WhitelistCounterBidEvaluator::from_vec(vec![whitelisted]);
        let tracker = BestSlotBidTracker::new(Box::new(evaluator));

        tracker.update(make_bid(1, 1_000_000, None));
        let counter = tracker.evaluate_counter_bid(1).unwrap();
        assert_eq!(counter, NON_WHITELISTED_COUNTER_BID);
    }

    #[test]
    fn test_best_bid_tracking() {
        let evaluator = WhitelistCounterBidEvaluator::from_vec(vec![]);
        let tracker = BestSlotBidTracker::new(Box::new(evaluator));

        tracker.update(make_bid(1, 100, Some(make_pubkey(1))));
        tracker.update(make_bid(1, 200, Some(make_pubkey(2))));
        tracker.update(make_bid(1, 150, Some(make_pubkey(3)))); // Lower, should not update

        let best = tracker.get_best_for_slot(1).unwrap();
        assert_eq!(best.value, U256::from(200));
        assert_eq!(best.builder_pubkey, Some(make_pubkey(2)));
    }
}
