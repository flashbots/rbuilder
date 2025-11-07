use std::sync::Arc;

use ahash::{HashMap, HashSet};
use alloy_primitives::U256;
use rbuilder_primitives::mev_boost::MevBoostRelayID;
use tokio_util::sync::CancellationToken;

use super::bidding_service_interface::*;

/// Bidding service that bids the true block value + subsidy to all relays.
pub struct NewTrueBlockValueBiddingService {
    slot_delta_to_start_bidding: time::Duration,
    relay_sets_subsidies: HashMap<RelaySet, U256>,
}

impl NewTrueBlockValueBiddingService {
    pub fn new(
        subsidy: U256,
        subsidy_overrides: HashMap<MevBoostRelayID, U256>,
        slot_delta_to_start_bidding: time::Duration,
        all_relays: RelaySet,
    ) -> Self {
        let mut default_relay_set: HashSet<MevBoostRelayID> =
            all_relays.relays().iter().cloned().collect();
        let mut relay_sets_subsidies = HashMap::default();
        for (relay, subsidy) in subsidy_overrides {
            default_relay_set.remove(&relay);
            relay_sets_subsidies.insert(RelaySet::new(vec![relay]), subsidy);
        }
        if !default_relay_set.is_empty() {
            relay_sets_subsidies.insert(
                RelaySet::new(default_relay_set.into_iter().collect()),
                subsidy,
            );
        }

        Self {
            slot_delta_to_start_bidding,
            relay_sets_subsidies,
        }
    }
}

pub struct NewTrueBlockValueSlotBidder {
    bid_start_time: time::OffsetDateTime,
    block_seal_handle: Box<dyn BlockSealInterfaceForSlotBidder + Send + Sync>,
    /// Will generate one bid per RelaySet
    relay_sets_subsidies: HashMap<RelaySet, U256>,
}

impl SlotBidder for NewTrueBlockValueSlotBidder {
    fn notify_new_built_block(&self, block_descriptor: BuiltBlockDescriptorForSlotBidder) {
        if time::OffsetDateTime::now_utc() < self.bid_start_time {
            return;
        }
        self.block_seal_handle.seal_bid(SlotBidderSealBidCommand {
            block_id: block_descriptor.id,
            seen_competition_bid: None,
            trigger_creation_time: Some(time::OffsetDateTime::now_utc()),
            payout_info: self
                .relay_sets_subsidies
                .iter()
                .map(|(relay_set, subsidy)| PayoutInfo {
                    relays: relay_set.clone(),
                    payout_tx_value: block_descriptor.true_block_value + subsidy,
                    subsidy: (*subsidy).try_into().unwrap(),
                })
                .collect(),
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
        })
    }

    fn relay_sets(&self) -> Vec<RelaySet> {
        self.relay_sets_subsidies.keys().cloned().collect()
    }

    fn observe_relay_bids(&self, _bid: ScrapedRelayBlockBidWithStats) {}

    fn update_new_landed_blocks_detected(&self, _landed_blocks: &[LandedBlockInfo]) {}

    fn update_failed_reading_new_landed_blocks(&self) {}
}
