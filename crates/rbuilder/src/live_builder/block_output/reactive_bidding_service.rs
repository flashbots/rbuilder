//! Reactive bidding service that tracks competition bids and outbids by increment.
//!
//! This bidding service uses MARKET-RELATIVE bidding:
//! 1. Tracks competition bids via `observe_relay_bids()`
//! 2. Bids: competition_bid + increment (capped at true_block_value)
//! 3. Respects max_bid cap and min_bid threshold
//! 4. Won't outbid protected builders (our own or whitelisted)

use std::sync::Arc;

use ahash::HashSet;
use alloy_primitives::{I256, U256};
use alloy_rpc_types_beacon::BlsPublicKey;
use parking_lot::Mutex;
use rbuilder_primitives::mev_boost::MevBoostRelayID;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{info, trace, warn};

use super::bidding_service_interface::*;

/// Default bid increment: 0.0001 ETH
const DEFAULT_INCREMENT: u64 = 100_000_000_000_000; // 0.0001 ETH in wei

/// Default max bid: 1 ETH  
const DEFAULT_MAX_BID: u64 = 1_000_000_000_000_000_000; // 1 ETH in wei

/// Default min bid threshold: 0.001 ETH
const DEFAULT_MIN_BID: u64 = 1_000_000_000_000_000; // 0.001 ETH in wei

/// A bidding service that uses MARKET-RELATIVE bidding:
/// 1. Tracks competition bids via observe_relay_bids()
/// 2. Bids: competition_bid + increment (capped at true_block_value)
/// 3. Respects max_bid cap and min_bid threshold
/// 4. Won't outbid protected builders
pub struct ReactiveBuilderFeeBiddingService {
    /// Amount to outbid competition by
    increment: U256,
    /// Maximum bid we'll ever make
    max_bid: U256,
    /// Minimum competition bid to react to
    min_bid: U256,
    /// Protected builder pubkeys (won't outbid these)
    protected_builders: HashSet<BlsPublicKey>,
    /// When to start bidding relative to slot start
    slot_delta_to_start_bidding: time::Duration,
    /// Relay sets to bid to
    relay_sets: Vec<RelaySet>,
    /// Shared state: best competition bid seen per slot
    best_competition_bid: Arc<Mutex<Option<ScrapedRelayBlockBidWithStats>>>,
}

impl ReactiveBuilderFeeBiddingService {
    /// Create with simple config (uses defaults for increment/max/min)
    pub fn new(
        _builder_fee: U256, // Kept for config compatibility, ignored in market-relative mode
        slot_delta_to_start_bidding: time::Duration,
        all_relays: RelaySet,
    ) -> Self {
        Self {
            increment: U256::from(DEFAULT_INCREMENT),
            max_bid: U256::from(DEFAULT_MAX_BID),
            min_bid: U256::from(DEFAULT_MIN_BID),
            protected_builders: HashSet::default(),
            slot_delta_to_start_bidding,
            relay_sets: vec![all_relays],
            best_competition_bid: Arc::new(Mutex::new(None)),
        }
    }

    /// Create with full market-relative config
    pub fn new_market_relative(
        increment: U256,
        max_bid: U256,
        min_bid: U256,
        protected_builders: Vec<BlsPublicKey>,
        slot_delta_to_start_bidding: time::Duration,
        all_relays: RelaySet,
    ) -> Self {
        Self {
            increment,
            max_bid,
            min_bid,
            protected_builders: protected_builders.into_iter().collect(),
            slot_delta_to_start_bidding,
            relay_sets: vec![all_relays],
            best_competition_bid: Arc::new(Mutex::new(None)),
        }
    }

    /// Create with per-relay overrides
    pub fn new_with_overrides(
        _default_builder_fee: U256,
        fee_overrides: ahash::HashMap<MevBoostRelayID, U256>,
        slot_delta_to_start_bidding: time::Duration,
        all_relays: RelaySet,
    ) -> Self {
        let mut default_relay_set: HashSet<MevBoostRelayID> =
            all_relays.relays().iter().cloned().collect();
        let mut relay_sets = Vec::new();

        for (relay, _fee) in fee_overrides.iter() {
            default_relay_set.remove(relay);
            relay_sets.push(RelaySet::new(vec![relay.clone()]));
        }

        if !default_relay_set.is_empty() {
            relay_sets.push(RelaySet::new(default_relay_set.into_iter().collect()));
        }

        Self {
            increment: U256::from(DEFAULT_INCREMENT),
            max_bid: U256::from(DEFAULT_MAX_BID),
            min_bid: U256::from(DEFAULT_MIN_BID),
            protected_builders: HashSet::default(),
            slot_delta_to_start_bidding,
            relay_sets,
            best_competition_bid: Arc::new(Mutex::new(None)),
        }
    }

    /// Check if a builder is protected (won't outbid)
    fn is_protected(&self, builder_pubkey: Option<&BlsPublicKey>) -> bool {
        builder_pubkey.is_some_and(|pk| self.protected_builders.contains(pk))
    }
}

impl BiddingService for ReactiveBuilderFeeBiddingService {
    fn create_slot_bidder(
        &self,
        slot_block_id: SlotBlockId,
        slot_timestamp: OffsetDateTime,
        block_seal_handle: Box<dyn BlockSealInterfaceForSlotBidder + Send + Sync>,
        _cancel: CancellationToken,
    ) -> Arc<dyn SlotBidder> {
        let bid_start_time = slot_timestamp + self.slot_delta_to_start_bidding;
        info!(
            slot = slot_block_id.slot,
            block = slot_block_id.block,
            increment = %self.increment,
            max_bid = %self.max_bid,
            bid_start_time = %bid_start_time,
            "🔧 Created market-relative slot bidder"
        );
        Arc::new(ReactiveSlotBidder {
            slot: slot_block_id.slot,
            bid_start_time,
            increment: self.increment,
            max_bid: self.max_bid,
            min_bid: self.min_bid,
            protected_builders: self.protected_builders.clone(),
            relay_sets: self.relay_sets.clone(),
            best_competition_bid: self.best_competition_bid.clone(),
            block_seal_handle,
            last_bid: Arc::new(Mutex::new(None)),
        })
    }

    fn relay_sets(&self) -> Vec<RelaySet> {
        self.relay_sets.clone()
    }

    /// Store incoming competition bids for reactive bidding (non-blocking)
    fn observe_relay_bids(&self, bid: ScrapedRelayBlockBidWithStats) {
        if let Some(mut guard) = self.best_competition_bid.try_lock() {
            if guard
                .as_ref()
                .map_or(true, |existing| bid.bid.value > existing.bid.value)
            {
                trace!(
                    slot = bid.bid.slot_number,
                    value = %bid.bid.value,
                    relay = %bid.bid.relay_name,
                    "Updated best competition bid"
                );
                *guard = Some(bid);
            }
        }
    }

    fn update_new_landed_blocks_detected(&self, _landed_blocks: &[LandedBlockInfo]) {}

    fn update_failed_reading_new_landed_blocks(&self) {}
}

pub struct ReactiveSlotBidder {
    slot: u64,
    bid_start_time: OffsetDateTime,
    increment: U256,
    max_bid: U256,
    min_bid: U256,
    protected_builders: HashSet<BlsPublicKey>,
    relay_sets: Vec<RelaySet>,
    best_competition_bid: Arc<Mutex<Option<ScrapedRelayBlockBidWithStats>>>,
    block_seal_handle: Box<dyn BlockSealInterfaceForSlotBidder + Send + Sync>,
    /// Track our last bid to avoid duplicate/lower bids
    last_bid: Arc<Mutex<Option<U256>>>,
}

impl ReactiveSlotBidder {
    fn is_protected(&self, builder_pubkey: Option<&BlsPublicKey>) -> bool {
        builder_pubkey.is_some_and(|pk| self.protected_builders.contains(pk))
    }

    fn format_builder(&self, pubkey: Option<&BlsPublicKey>) -> String {
        pubkey
            .map(|pk| {
                let hex = format!("{:?}", pk);
                if hex.len() > 14 {
                    format!("{}...{}", &hex[..10], &hex[hex.len() - 4..])
                } else {
                    hex
                }
            })
            .unwrap_or_else(|| "unknown".to_string())
    }
}

impl SlotBidder for ReactiveSlotBidder {
    fn notify_new_built_block(&self, block_descriptor: BuiltBlockDescriptorForSlotBidder) {
        let true_block_value = block_descriptor.true_block_value;

        info!(
            slot = self.slot,
            true_block_value = %true_block_value,
            "📦 New block built, evaluating market-relative bid"
        );

        // Extract competition bid info
        let competition_bid_info = self
            .best_competition_bid
            .try_lock()
            .and_then(|guard| {
                guard
                    .as_ref()
                    .filter(|b| b.bid.slot_number == self.slot)
                    .cloned()
            });

        let (competition_value, competition_builder, competition_relay) = competition_bid_info
            .as_ref()
            .map(|b| (b.bid.value, b.bid.builder_pubkey, b.bid.relay_name.clone()))
            .unwrap_or_else(|| (U256::ZERO, None, "none".to_string()));

        // Check if top bidder is protected
        if self.is_protected(competition_builder.as_ref()) {
            info!(
                slot = self.slot,
                builder = %self.format_builder(competition_builder.as_ref()),
                "⏭️ Skipping - top bidder is protected"
            );
            return;
        }

        // Check minimum threshold
        if competition_value < self.min_bid && competition_value > U256::ZERO {
            trace!(
                slot = self.slot,
                competition_value = %competition_value,
                min_bid = %self.min_bid,
                "⏭️ Skipping - competition below minimum threshold"
            );
            return;
        }

        // Calculate our bid: competition + increment, capped at true_block_value and max_bid
        let target_bid = competition_value.saturating_add(self.increment);
        let bid_value = target_bid.min(true_block_value).min(self.max_bid);

        // Check if bid would exceed max
        if target_bid > self.max_bid {
            warn!(
                slot = self.slot,
                target_bid = %target_bid,
                max_bid = %self.max_bid,
                "⏭️ Skipping - bid would exceed max cap"
            );
            return;
        }

        // Check if we can afford this bid (have enough block value)
        if bid_value > true_block_value {
            warn!(
                slot = self.slot,
                bid_value = %bid_value,
                true_block_value = %true_block_value,
                "⏭️ Skipping - insufficient block value"
            );
            return;
        }

        // Check if we already bid this amount or higher
        {
            let last = self.last_bid.lock();
            if let Some(last_bid) = *last {
                if bid_value <= last_bid {
                    trace!(
                        slot = self.slot,
                        bid_value = %bid_value,
                        last_bid = %last_bid,
                        "⏭️ Skipping - already bid higher"
                    );
                    return;
                }
            }
        }

        // Update our last bid
        *self.last_bid.lock() = Some(bid_value);

        // Calculate builder profit (what we retain)
        let builder_profit = true_block_value.saturating_sub(bid_value);

        info!(
            slot = self.slot,
            "╔══════════════════════════════════════════════════════════════"
        );
        info!(
            slot = self.slot,
            prev_bid_wei = %competition_value,
            prev_bidder = %self.format_builder(competition_builder.as_ref()),
            prev_relay = %competition_relay,
            "║ 📊 COMPETITION:   {} wei from {} via {}",
            competition_value,
            self.format_builder(competition_builder.as_ref()),
            competition_relay
        );
        info!(
            slot = self.slot,
            our_bid_wei = %bid_value,
            increment = %self.increment,
            builder_profit = %builder_profit,
            "║ 🎯 OUR BID:       {} wei (comp + {} = {} profit)",
            bid_value,
            self.increment,
            builder_profit
        );
        info!(
            slot = self.slot,
            "╚══════════════════════════════════════════════════════════════"
        );

        // Build competition context
        let competition_context = competition_bid_info
            .as_ref()
            .map(|b| CompetitionBidContext {
                seen_competition_bid: Some(BidWithInfo::from(&b.bid)),
                triggering_bid_source_info: Some(BidSourceInfo::from(&b.bid)),
            })
            .unwrap_or_else(CompetitionBidContext::no_competition_bid);

        // Calculate subsidy (negative = profit retained)
        let subsidy = I256::try_from(builder_profit)
            .unwrap_or(I256::ZERO)
            .checked_neg()
            .unwrap_or(I256::ZERO);

        let payout_info: Vec<PayoutInfo> = self
            .relay_sets
            .iter()
            .map(|relay_set| PayoutInfo {
                relays: relay_set.clone(),
                payout_tx_value: bid_value,
                subsidy,
            })
            .collect();

        self.block_seal_handle.seal_bid(SlotBidderSealBidCommand {
            block_id: block_descriptor.id,
            trigger_creation_time: Some(OffsetDateTime::now_utc()),
            competition_bid_context: competition_context,
            payout_info,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reactive_bidding_service_creation() {
        let relay_set = RelaySet::new(vec!["test-relay".to_string()]);
        let service = ReactiveBuilderFeeBiddingService::new(
            U256::from(10_000_000_000_000_000u64), // ignored in market-relative mode
            time::Duration::milliseconds(-8000),
            relay_set,
        );

        assert_eq!(service.relay_sets().len(), 1);
        assert_eq!(service.increment, U256::from(DEFAULT_INCREMENT));
    }

    #[test]
    fn test_market_relative_bid_calculation() {
        let competition_value = U256::from(50_000_000_000_000_000u64); // 0.05 ETH
        let increment = U256::from(100_000_000_000_000u64); // 0.0001 ETH
        let true_block_value = U256::from(100_000_000_000_000_000u64); // 0.1 ETH

        // bid = competition + increment
        let target_bid = competition_value.saturating_add(increment);
        let bid_value = target_bid.min(true_block_value);

        assert_eq!(bid_value, U256::from(50_100_000_000_000_000u64)); // 0.0501 ETH

        // builder profit = true_value - bid
        let builder_profit = true_block_value.saturating_sub(bid_value);
        assert_eq!(builder_profit, U256::from(49_900_000_000_000_000u64)); // ~0.0499 ETH
    }

    #[test]
    fn test_bid_capped_at_true_value() {
        let competition_value = U256::from(99_000_000_000_000_000u64); // 0.099 ETH
        let increment = U256::from(10_000_000_000_000_000u64); // 0.01 ETH
        let true_block_value = U256::from(100_000_000_000_000_000u64); // 0.1 ETH

        let target_bid = competition_value.saturating_add(increment); // 0.109 ETH
        let bid_value = target_bid.min(true_block_value); // capped at 0.1 ETH

        assert_eq!(bid_value, true_block_value);
    }
}
