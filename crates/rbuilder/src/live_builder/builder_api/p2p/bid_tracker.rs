use alloy_primitives::{BlockHash, B256};
use parking_lot::RwLock;
use rbuilder_primitives::epbs::SignedExecutionPayloadBid;
use std::collections::HashMap;
use tracing::debug;

// TODO: revisit key construction
/// Key for tracking bids
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BidKey {
    slot: u64,
    parent_block_hash: BlockHash,
    parent_block_root: B256,
}

impl BidKey {
    fn from_bid(bid: &rbuilder_primitives::epbs::ExecutionPayloadBid) -> Self {
        Self {
            slot: bid.slot,
            parent_block_hash: bid.parent_block_hash,
            parent_block_root: bid.parent_block_root,
        }
    }
}

/// Tracks competing bids and our own bids per slot from execution_payload_bid SSE to[ic]
#[derive(Debug)]
pub struct BidTracker {
    /// Highest bid seen per (slot, parent_hash, parent_root).
    highest_bids: RwLock<HashMap<BidKey, SignedExecutionPayloadBid>>,
    /// Our own submitted bids: slot -> latest bid.
    our_bids: RwLock<HashMap<u64, SignedExecutionPayloadBid>>,
    // TODO: maybe we can remove this but keep for now
    /// Our builder index for identifying our own bids.
    our_builder_index: u64,
}

impl BidTracker {
    pub fn new(our_builder_index: u64) -> Self {
        Self {
            highest_bids: RwLock::new(HashMap::new()),
            our_bids: RwLock::new(HashMap::new()),
            our_builder_index,
        }
    }

    /// Process an incoming bid from the SSE stream.
    /// Returns true if this bid is the new highest for its key.
    pub fn on_bid_received(&self, bid: &SignedExecutionPayloadBid) -> bool {
        let key = BidKey::from_bid(&bid.message);
        let mut highest = self.highest_bids.write();

        let is_new_highest = match highest.get(&key) {
            Some(existing) => bid.message.value > existing.message.value,
            None => true,
        };

        if is_new_highest {
            debug!(
                slot = bid.message.slot,
                builder_index = bid.message.builder_index,
                value = bid.message.value,
                "New highest bid received"
            );
            highest.insert(key, bid.clone());
        }

        is_new_highest
    }

    /// Record a bid we submitted.
    pub fn on_bid_submitted(&self, bid: &SignedExecutionPayloadBid) {
        let slot = bid.message.slot;
        self.our_bids.write().insert(slot, bid.clone());
        // Also track it as potentially the highest
        self.on_bid_received(bid);
    }

    /// Get the highest competing bid for a given slot/parent combination.
    pub fn highest_bid(
        &self,
        slot: u64,
        parent_hash: &BlockHash,
        parent_root: &B256,
    ) -> Option<SignedExecutionPayloadBid> {
        let key = BidKey {
            slot,
            parent_block_hash: *parent_hash,
            parent_block_root: *parent_root,
        };
        self.highest_bids.read().get(&key).cloned()
    }

    /// Get our latest submitted bid for a slot.
    pub fn our_bid(&self, slot: u64) -> Option<SignedExecutionPayloadBid> {
        self.our_bids.read().get(&slot).cloned()
    }

    /// Check if we are currently the highest bidder for a slot/parent.
    pub fn are_we_winning(
        &self,
        slot: u64,
        parent_hash: &BlockHash,
        parent_root: &B256,
    ) -> bool {
        let key = BidKey {
            slot,
            parent_block_hash: *parent_hash,
            parent_block_root: *parent_root,
        };
        self.highest_bids
            .read()
            .get(&key)
            .map(|bid| bid.message.builder_index == self.our_builder_index)
            .unwrap_or(false)
    }

    /// Clean up entries older than current_slot.
    pub fn cleanup(&self, current_slot: u64) {
        self.highest_bids
            .write()
            .retain(|key, _| key.slot >= current_slot.saturating_sub(2));
        self.our_bids
            .write()
            .retain(|&slot, _| slot >= current_slot.saturating_sub(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use alloy_rpc_types_beacon::BlsSignature;

    fn make_bid(slot: u64, builder_index: u64, value: u64) -> SignedExecutionPayloadBid {
        SignedExecutionPayloadBid {
            message: rbuilder_primitives::epbs::ExecutionPayloadBid {
                parent_block_hash: BlockHash::ZERO,
                parent_block_root: B256::ZERO,
                block_hash: BlockHash::ZERO,
                prev_randao: B256::ZERO,
                fee_recipient: Address::ZERO,
                gas_limit: 30_000_000,
                builder_index,
                slot,
                value,
                execution_payment: 0,
                blob_kzg_commitments: vec![],
            },
            signature: BlsSignature::default(),
        }
    }

    #[test]
    fn test_bid_tracking() {
        let tracker = BidTracker::new(1);

        // 1st bid from builder 2
        let bid1 = make_bid(100, 2, 1000);
        assert!(tracker.on_bid_received(&bid1));

        // higher  bid from builder 3
        let bid2 = make_bid(100, 3, 2000);
        assert!(tracker.on_bid_received(&bid2));

        // lower bid should not replace
        let bid3 = make_bid(100, 4, 500);
        assert!(!tracker.on_bid_received(&bid3));

        let highest = tracker
            .highest_bid(100, &BlockHash::ZERO, &B256::ZERO)
            .unwrap();
        assert_eq!(highest.message.value, 2000);
    }

    #[test]
    fn test_our_bid_tracking() {
        let tracker = BidTracker::new(1);

        let our_bid = make_bid(100, 1, 1500);
        tracker.on_bid_submitted(&our_bid);

        assert!(tracker.our_bid(100).is_some());
        assert!(tracker.are_we_winning(100, &BlockHash::ZERO, &B256::ZERO));

        // someone outbided us
        let higher_bid = make_bid(100, 2, 2000);
        tracker.on_bid_received(&higher_bid);
        assert!(!tracker.are_we_winning(100, &BlockHash::ZERO, &B256::ZERO));
    }

    #[test]
    fn test_cleanup() {
        let tracker = BidTracker::new(1);
        tracker.on_bid_submitted(&make_bid(10, 1, 100));
        tracker.on_bid_submitted(&make_bid(20, 1, 200));

        tracker.cleanup(20);
        assert!(tracker.our_bid(10).is_none());
        assert!(tracker.our_bid(20).is_some());
    }
}
