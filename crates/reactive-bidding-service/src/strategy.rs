//! Bidding strategy module.
//!
//! Implements reactive bidding logic:
//! - Track the best bid across all relays
//! - If top bidder is not us/whitelisted, outbid by increment
//! - Respect max bid cap
//! - Send counter-bid commands through a channel for consumption by other crates

use std::collections::HashSet;
use std::sync::Arc;

use alloy_primitives::U256;
use bid_scraper::types::ScrapedRelayBlockBid;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::config::BiddingConfig;

/// Current best bid state across all relays
#[derive(Debug, Clone)]
pub struct BestBidState {
    pub slot: u64,
    pub block_number: u64,
    pub value: U256,
    pub value_eth: f64,
    pub builder: Option<String>,
    pub relay: String,
}

/// A counter-bid command to be sent to the builder
#[derive(Debug, Clone)]
pub struct CounterBid {
    /// Slot number
    pub slot: u64,
    /// Block number
    pub block_number: u64,
    /// Our counter-bid value in wei
    pub bid_value: U256,
    /// Our counter-bid value in ETH (for display)
    pub bid_value_eth: f64,
    /// The bid we're countering
    pub countering_value: U256,
    /// The builder we're outbidding
    pub countering_builder: Option<String>,
    /// The relay this bid came from
    pub relay: String,
    /// Reason for this bid
    pub reason: String,
}

/// Receiver end for counter-bid commands
pub type CounterBidReceiver = mpsc::UnboundedReceiver<CounterBid>;

/// Sender end for counter-bid commands (internal use)
pub type CounterBidSender = mpsc::UnboundedSender<CounterBid>;

/// Reactive bidding strategy
///
/// Always evaluates incoming bids and sends counter-bids through a channel
/// when appropriate. The channel can be consumed by other crates (e.g., rbuilder).
pub struct BiddingStrategy {
    config: BiddingConfig,
    protected_builders: HashSet<String>,
    current_best: Mutex<Option<BestBidState>>,
    our_last_bid: Mutex<Option<U256>>,
    counter_bid_tx: CounterBidSender,
}

impl BiddingStrategy {
    /// Create a new bidding strategy and return it along with the counter-bid receiver
    pub fn new(config: BiddingConfig) -> (Arc<Self>, CounterBidReceiver) {
        let protected_builders = config.protected_builders();
        let (tx, rx) = mpsc::unbounded_channel();

        info!(
            "Bidding strategy initialized: increment={:.6} ETH, max={:.4} ETH, protected_builders={}",
            config.increment_eth,
            config.max_bid_eth,
            protected_builders.len()
        );

        let strategy = Arc::new(Self {
            config,
            protected_builders,
            current_best: Mutex::new(None),
            our_last_bid: Mutex::new(None),
            counter_bid_tx: tx,
        });

        (strategy, rx)
    }

    /// Check if a builder address is protected (ours or whitelisted)
    fn is_protected(&self, builder: &Option<String>) -> bool {
        match builder {
            Some(addr) => self.protected_builders.contains(&addr.to_lowercase()),
            None => false,
        }
    }

    /// Process a new bid - evaluates and sends counter-bid if appropriate
    /// Returns true if a counter-bid was sent
    pub fn on_bid(&self, bid: &ScrapedRelayBlockBid) -> bool {
        let value_eth = bid.value.to_string().parse::<f64>().unwrap_or(0.0) / 1e18;
        let builder = bid.builder_pubkey.map(|b| format!("0x{}", b));

        let mut current_best = self.current_best.lock();

        // Check if this is a new block - reset state
        let is_new_block = current_best
            .as_ref()
            .map(|b| bid.block_number > b.block_number)
            .unwrap_or(true);

        if is_new_block {
            *self.our_last_bid.lock() = None;
        }

        // Check if this bid is better than current best
        let is_new_best = current_best
            .as_ref()
            .map(|b| bid.value > b.value && bid.block_number >= b.block_number)
            .unwrap_or(true);

        if !is_new_best {
            return false;
        }

        // Update best bid state
        let new_best = BestBidState {
            slot: bid.slot_number,
            block_number: bid.block_number,
            value: bid.value,
            value_eth,
            builder: builder.clone(),
            relay: bid.relay_name.clone(),
        };
        *current_best = Some(new_best.clone());
        drop(current_best);

        // Evaluate and potentially send counter-bid
        self.evaluate_and_send(&new_best)
    }

    /// Evaluate whether we should counter-bid and send if appropriate
    fn evaluate_and_send(&self, best: &BestBidState) -> bool {
        // Check if top bidder is protected
        if self.is_protected(&best.builder) {
            tracing::trace!(
                "Skip: top bidder {} is protected",
                best.builder.as_deref().unwrap_or("unknown")
            );
            return false;
        }

        // Check if bid is above minimum threshold
        if best.value < self.config.min_bid_wei() {
            tracing::trace!(
                "Skip: best bid {:.6} ETH below minimum",
                best.value_eth
            );
            return false;
        }

        // Calculate our bid: current best + increment
        let our_bid = best.value.saturating_add(self.config.increment_wei());
        let our_bid_eth = our_bid.to_string().parse::<f64>().unwrap_or(0.0) / 1e18;

        // Check max cap
        if our_bid > self.config.max_bid_wei() {
            tracing::trace!(
                "Skip: calculated bid {:.6} ETH exceeds max cap",
                our_bid_eth
            );
            return false;
        }

        // Check if we already bid this amount or higher
        {
            let last_bid = self.our_last_bid.lock();
            if let Some(last) = *last_bid {
                if our_bid <= last {
                    tracing::trace!("Skip: already bid higher");
                    return false;
                }
            }
        }

        // Update our last bid
        *self.our_last_bid.lock() = Some(our_bid);

        let builder_short = best
            .builder
            .as_ref()
            .map(|b| {
                if b.len() > 14 {
                    format!("{}...{}", &b[..10], &b[b.len() - 4..])
                } else {
                    b.clone()
                }
            })
            .unwrap_or_else(|| "unknown".to_string());

        let reason = format!(
            "Outbidding {} by +{:.6} ETH",
            builder_short,
            self.config.increment_eth
        );

        debug!(
            "💰 COUNTER-BID: slot={} block={} our_bid={:.6} ETH (vs {:.6} ETH from {} via {})",
            best.slot,
            best.block_number,
            our_bid_eth,
            best.value_eth,
            builder_short,
            best.relay
        );

        let counter_bid = CounterBid {
            slot: best.slot,
            block_number: best.block_number,
            bid_value: our_bid,
            bid_value_eth: our_bid_eth,
            countering_value: best.value,
            countering_builder: best.builder.clone(),
            relay: best.relay.clone(),
            reason,
        };

        // Send through channel - ignore error if receiver dropped
        let _ = self.counter_bid_tx.send(counter_bid);
        true
    }

    /// Get the current best bid state
    pub fn current_best(&self) -> Option<BestBidState> {
        self.current_best.lock().clone()
    }

    /// Get the configuration
    pub fn config(&self) -> &BiddingConfig {
        &self.config
    }
}

/// Create a bidding strategy and return the counter-bid receiver
pub fn create_strategy(config: BiddingConfig) -> (Arc<BiddingStrategy>, CounterBidReceiver) {
    BiddingStrategy::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::FixedBytes;

    fn make_bid(value_eth: f64, builder: Option<&str>) -> ScrapedRelayBlockBid {
        let value_wei = (value_eth * 1e18) as u128;
        ScrapedRelayBlockBid {
            seen_time: 0.0,
            relay_name: "test-relay".to_string(),
            publisher_name: "test".to_string(),
            value: U256::from(value_wei),
            slot_number: 1000,
            block_number: 2000,
            block_hash: Default::default(),
            parent_hash: Default::default(),
            builder_pubkey: builder.map(|_| FixedBytes::default()),
            proposer_fee_recipient: Default::default(),
            proposer_pubkey: Default::default(),
            optimistic_submission: false,
        }
    }

    #[tokio::test]
    async fn test_counter_bid_sent() {
        let config = BiddingConfig {
            increment_eth: 0.0001,
            max_bid_eth: 0.1,
            min_bid_eth: 0.0001,
            ..Default::default()
        };
        let (strategy, mut rx) = BiddingStrategy::new(config);

        let bid = make_bid(0.01, None);
        let sent = strategy.on_bid(&bid);

        assert!(sent, "Should have sent a counter-bid");

        // Check the counter-bid was received
        let counter_bid = rx.try_recv().expect("Should have received counter-bid");
        assert!(counter_bid.bid_value_eth > 0.01);
        assert!(counter_bid.bid_value_eth < 0.0102); // increment is 0.0001
    }

    #[tokio::test]
    async fn test_respects_max_cap() {
        let config = BiddingConfig {
            increment_eth: 0.0001,
            max_bid_eth: 0.01, // Low max cap
            min_bid_eth: 0.0001,
            ..Default::default()
        };
        let (strategy, mut rx) = BiddingStrategy::new(config);

        // Bid at max cap - counter-bid would exceed
        let bid = make_bid(0.01, None);
        let sent = strategy.on_bid(&bid);

        assert!(!sent, "Should not bid when it would exceed max cap");
        assert!(rx.try_recv().is_err(), "Should not have received counter-bid");
    }

    #[tokio::test]
    async fn test_protected_builder_not_outbid() {
        let config = BiddingConfig {
            increment_eth: 0.0001,
            max_bid_eth: 0.1,
            min_bid_eth: 0.0001,
            our_builders: vec!["0x0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".to_string()],
            ..Default::default()
        };
        let (strategy, mut rx) = BiddingStrategy::new(config);

        // Bid from our builder
        let bid = make_bid(0.01, Some("our_builder"));
        let sent = strategy.on_bid(&bid);

        // The builder pubkey in make_bid is FixedBytes::default() which is all zeros
        // Our protected list includes the all-zeros pubkey
        assert!(!sent, "Should not outbid protected builder");
        assert!(rx.try_recv().is_err(), "Should not have received counter-bid");
    }
}
