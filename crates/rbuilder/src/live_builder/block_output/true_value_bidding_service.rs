use std::sync::Arc;

use ahash::{HashMap, HashSet};
use alloy_primitives::U256;
use rbuilder_primitives::mev_boost::MevBoostRelayID;
use tokio_util::sync::CancellationToken;

use super::bidding_service_interface::*;

fn max_bid_for_relay_set(
    relays: &[MevBoostRelayID],
    caps: &HashMap<MevBoostRelayID, U256>,
) -> Option<U256> {
    let mut min_cap: Option<U256> = None;
    for relay in relays {
        let cap = caps.get(relay)?;
        min_cap = Some(match min_cap {
            Some(current) => current.min(*cap),
            None => *cap,
        });
    }
    min_cap
}

fn clamp_payout(value: U256, max_bid: Option<U256>) -> U256 {
    match max_bid {
        Some(cap) => value.min(cap),
        None => value,
    }
}

/// Parsed configuration for TrueBlockValueBiddingService.
#[derive(Debug, Clone, PartialEq)]
pub struct TrueBlockValueBiddingServiceConfig {
    /// Default subsidy value.
    pub subsidy: U256,
    /// Per-relay subsidy overrides.
    pub subsidy_overrides: HashMap<MevBoostRelayID, U256>,
    /// When the sample bidder will start bidding.
    pub slot_delta_to_start_bidding: time::Duration,
}

/// Bidding service that bids the true block value + subsidy to all relays.
pub struct NewTrueBlockValueBiddingService {
    slot_delta_to_start_bidding: time::Duration,
    relay_sets_subsidies: HashMap<RelaySet, U256>,
    relay_sets_max_bids: HashMap<RelaySet, Option<U256>>,
}

impl NewTrueBlockValueBiddingService {
    pub fn new(
        config: &TrueBlockValueBiddingServiceConfig,
        all_relays: RelaySet,
        relay_max_bids: &HashMap<MevBoostRelayID, U256>,
    ) -> eyre::Result<Self> {
        let mut default_relay_set: HashSet<MevBoostRelayID> =
            all_relays.relays().iter().cloned().collect();
        let mut relay_sets_subsidies = HashMap::default();
        let mut relay_sets_max_bids = HashMap::default();

        for (relay, subsidy) in config.subsidy_overrides.clone() {
            default_relay_set.remove(&relay);
            let relay_set = RelaySet::new(vec![relay.clone()]);
            relay_sets_subsidies.insert(relay_set.clone(), subsidy);
            relay_sets_max_bids.insert(relay_set, max_bid_for_relay_set(&[relay], relay_max_bids));
        }
        if !default_relay_set.is_empty() {
            let relay_set = RelaySet::new(default_relay_set.into_iter().collect());
            relay_sets_subsidies.insert(relay_set.clone(), config.subsidy);
            relay_sets_max_bids.insert(
                relay_set,
                max_bid_for_relay_set(relay_set.relays(), relay_max_bids),
            );
        }

        Ok(Self {
            slot_delta_to_start_bidding: config.slot_delta_to_start_bidding,
            relay_sets_subsidies,
            relay_sets_max_bids,
        })
    }
}

pub struct NewTrueBlockValueSlotBidder {
    bid_start_time: time::OffsetDateTime,
    block_seal_handle: Box<dyn BlockSealInterfaceForSlotBidder + Send + Sync>,
    /// Will generate one bid per RelaySet
    relay_sets_subsidies: HashMap<RelaySet, U256>,
    relay_sets_max_bids: HashMap<RelaySet, Option<U256>>,
}

impl SlotBidder for NewTrueBlockValueSlotBidder {
    fn notify_new_built_block(&self, block_descriptor: BuiltBlockDescriptorForSlotBidder) {
        if time::OffsetDateTime::now_utc() < self.bid_start_time {
            return;
        }
        self.block_seal_handle.seal_bid(SlotBidderSealBidCommand {
            block_id: block_descriptor.id,
            trigger_creation_time: Some(time::OffsetDateTime::now_utc()),
            payout_info: self
                .relay_sets_subsidies
                .iter()
                .map(|(relay_set, subsidy)| {
                    let uncapped = block_descriptor.true_block_value + subsidy;
                    let max_bid = self.relay_sets_max_bids.get(relay_set).copied().flatten();
                    PayoutInfo {
                        relays: relay_set.clone(),
                        payout_tx_value: clamp_payout(uncapped, max_bid),
                        subsidy: (*subsidy).try_into().unwrap(),
                    }
                })
                .collect(),
            competition_bid_context: CompetitionBidContext::no_competition_bid(),
        })
    }
}

impl BiddingService for NewTrueBlockValueBiddingService {
    fn create_slot_bidder(
        &self,
        _slot_block_id: SlotBlockId,
        slot_timestamp: time::OffsetDateTime,
        block_seal_handle: Box<dyn BlockSealInterfaceForSlotBidder + Send + Sync>,
        _cancel: CancellationToken,
    ) -> Arc<dyn SlotBidder> {
        let bid_start_time = slot_timestamp + self.slot_delta_to_start_bidding;
        Arc::new(NewTrueBlockValueSlotBidder {
            bid_start_time,
            block_seal_handle,
            relay_sets_subsidies: self.relay_sets_subsidies.clone(),
            relay_sets_max_bids: self.relay_sets_max_bids.clone(),
        })
    }

    fn relay_sets(&self) -> Vec<RelaySet> {
        self.relay_sets_subsidies.keys().cloned().collect()
    }

    fn observe_relay_bids(&self, _bid: Vec<ScrapedRelayBlockBidWithStats>) {}

    fn update_new_landed_blocks_detected(&self, _landed_blocks: &[LandedBlockInfo]) {}

    fn update_failed_reading_new_landed_blocks(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::utils::parse_ether;

    #[test]
    fn max_bid_for_relay_set_requires_cap_on_every_relay() {
        let relay_a = MevBoostRelayID::from("relay_a");
        let relay_b = MevBoostRelayID::from("relay_b");
        let caps = HashMap::from([(relay_a.clone(), parse_ether("0.05").unwrap())]);
        assert_eq!(
            max_bid_for_relay_set(&[relay_a.clone(), relay_b.clone()], &caps),
            None
        );
        assert_eq!(
            max_bid_for_relay_set(&[relay_a.clone()], &caps),
            Some(parse_ether("0.05").unwrap())
        );
    }

    #[test]
    fn clamp_payout_respects_relay_max_bid() {
        let cap = parse_ether("0.05").unwrap();
        let high = parse_ether("1").unwrap();
        assert_eq!(clamp_payout(high, Some(cap)), cap);
        assert_eq!(clamp_payout(cap, None), cap);
    }
}
