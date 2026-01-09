use std::sync::Arc;

use ahash::{HashMap, HashSet};
use alloy_primitives::U256;
use rbuilder_primitives::mev_boost::MevBoostRelayID;
use tokio_util::sync::CancellationToken;

use super::bidding_service_interface::*;

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
}

impl NewTrueBlockValueBiddingService {
    pub fn new(
        config: &TrueBlockValueBiddingServiceConfig,
        all_relays: RelaySet,
    ) -> eyre::Result<Self> {
        let mut default_relay_set: HashSet<MevBoostRelayID> =
            all_relays.relays().iter().cloned().collect();
        let mut relay_sets_subsidies = HashMap::default();

        for (relay, subsidy) in config.subsidy_overrides.clone() {
            default_relay_set.remove(&relay);
            relay_sets_subsidies.insert(RelaySet::new(vec![relay]), subsidy);
        }
        if !default_relay_set.is_empty() {
            relay_sets_subsidies.insert(
                RelaySet::new(default_relay_set.into_iter().collect()),
                config.subsidy,
            );
        }

        Ok(Self {
            slot_delta_to_start_bidding: config.slot_delta_to_start_bidding,
            relay_sets_subsidies,
        })
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
        })
    }

    fn relay_sets(&self) -> Vec<RelaySet> {
        self.relay_sets_subsidies.keys().cloned().collect()
    }

    fn observe_relay_bids(&self, _bid: ScrapedRelayBlockBidWithStats) {}

    fn update_new_landed_blocks_detected(&self, _landed_blocks: &[LandedBlockInfo]) {}

    fn update_failed_reading_new_landed_blocks(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        building::builders::BuiltBlockId,
        live_builder::block_output::bidding_service_interface::{
            BlockSealInterfaceForSlotBidder, BuiltBlockDescriptorForSlotBidder,
            CompetitionBidContext, SlotBidderSealBidCommand, SlotBlockId,
        },
    };
    use alloy_primitives::{BlockHash, I256};
    use parking_lot::Mutex;
    use std::{collections::HashMap as StdHashMap, sync::Arc};
    use time::{Duration, OffsetDateTime};

    #[derive(Clone, Default)]
    struct RecordingSealHandle {
        sent: Arc<Mutex<Vec<SlotBidderSealBidCommand>>>,
    }

    impl RecordingSealHandle {
        fn new() -> Self {
            Self::default()
        }

        fn sent_commands(&self) -> Vec<SlotBidderSealBidCommand> {
            self.sent.lock().clone()
        }
    }

    impl BlockSealInterfaceForSlotBidder for RecordingSealHandle {
        fn seal_bid(&self, bid: SlotBidderSealBidCommand) {
            self.sent.lock().push(bid);
        }
    }

    fn relay(name: &str) -> MevBoostRelayID {
        name.to_string()
    }

    fn descriptor_with_value(value: u64) -> BuiltBlockDescriptorForSlotBidder {
        BuiltBlockDescriptorForSlotBidder {
            true_block_value: U256::from(value),
            id: BuiltBlockId(value),
            creation_time: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn relay_sets_respect_overrides() {
        let mut overrides = HashMap::default();
        overrides.insert(relay("solo"), U256::from(5));

        let service = NewTrueBlockValueBiddingService::new(
            U256::from(1),
            overrides,
            Duration::ZERO,
            RelaySet::new(vec![relay("solo"), relay("shared_a"), relay("shared_b")]),
        );

        let relay_sets = service.relay_sets();
        assert_eq!(relay_sets.len(), 2);
        assert!(relay_sets.contains(&RelaySet::new(vec![relay("solo")])));
        assert!(relay_sets.contains(&RelaySet::new(vec![relay("shared_a"), relay("shared_b")])));
    }

    #[test]
    fn slot_bidder_waits_until_start_time() {
        let service = NewTrueBlockValueBiddingService::new(
            U256::from(1),
            HashMap::default(),
            Duration::seconds(60),
            RelaySet::new(vec![relay("relay")]),
        );
        let recording_handle = RecordingSealHandle::new();
        let bidder = service.create_slot_bidder(
            SlotBlockId::new(1, 1, BlockHash::ZERO),
            OffsetDateTime::now_utc(),
            Box::new(recording_handle.clone()),
            CancellationToken::new(),
        );

        bidder.notify_new_built_block(descriptor_with_value(42));
        assert!(recording_handle.sent_commands().is_empty());
    }

    #[test]
    fn slot_bidder_emits_payout_for_each_relay_set() {
        let mut overrides = HashMap::default();
        overrides.insert(relay("solo"), U256::from(3));

        let service = NewTrueBlockValueBiddingService::new(
            U256::from(1),
            overrides,
            Duration::ZERO,
            RelaySet::new(vec![relay("solo"), relay("shared")]),
        );

        let recording_handle = RecordingSealHandle::new();
        let bidder = service.create_slot_bidder(
            SlotBlockId::new(1, 1, BlockHash::ZERO),
            OffsetDateTime::now_utc() - Duration::seconds(1),
            Box::new(recording_handle.clone()),
            CancellationToken::new(),
        );
        let descriptor = descriptor_with_value(100);

        bidder.notify_new_built_block(descriptor.clone());

        let commands = recording_handle.sent_commands();
        assert_eq!(commands.len(), 1);
        let command = &commands[0];
        assert_eq!(command.block_id, descriptor.id);
        assert_eq!(
            command.competition_bid_context,
            CompetitionBidContext::no_competition_bid()
        );
        assert!(command.trigger_creation_time.is_some());

        let payouts: StdHashMap<RelaySet, (U256, I256)> = command
            .payout_info
            .iter()
            .map(|info| (info.relays.clone(), (info.payout_tx_value, info.subsidy)))
            .collect();

        assert_eq!(payouts.len(), 2);
        assert_eq!(
            payouts
                .get(&RelaySet::new(vec![relay("solo")]))
                .copied()
                .unwrap(),
            (descriptor.true_block_value + U256::from(3), I256::from(3))
        );
        assert_eq!(
            payouts
                .get(&RelaySet::new(vec![relay("shared")]))
                .copied()
                .unwrap(),
            (descriptor.true_block_value + U256::from(1), I256::from(1))
        );
    }
}
