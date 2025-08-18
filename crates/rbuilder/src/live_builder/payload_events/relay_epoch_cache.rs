use crate::{
    mev_boost::{RelayError, ValidatorSlotData},
    primitives::mev_boost::{MevBoostRelayID, MevBoostRelaySlotInfoProvider},
    telemetry::{inc_conn_relay_errors, inc_other_relay_errors, inc_too_many_req_relay_errors},
};
use ahash::{HashMap, HashSet};
use alloy_primitives::Address;
use parking_lot::RwLock;
use primitive_types::H384;
use std::{sync::Arc, time::Duration};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::*;

/// Info about a slot obtained from a relay.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Default)]
pub struct SlotData {
    /// fee recipient the validator chose.
    pub fee_recipient: Address,
    pub gas_limit: u64,
    /// Selected registered validator for the slot key.
    pub pubkey: H384,
}

/// Validator slot data by relay.
#[derive(Clone, Debug)]
struct RelayValidatorSlotDataCache(
    Arc<RwLock<HashMap<MevBoostRelayID, HashMap<u64, ValidatorSlotData>>>>,
);

impl Default for RelayValidatorSlotDataCache {
    fn default() -> Self {
        Self(Arc::new(RwLock::new(Default::default())))
    }
}

impl RelayValidatorSlotDataCache {
    async fn update(&self, clients: &[MevBoostRelaySlotInfoProvider]) {
        for client in clients {
            let registrations = match client.get_current_epoch_validators().await {
                Ok(data) => data,
                Err(error) => {
                    let relay = client.id();
                    warn!(%relay, ?error, "Error updating validator registrations");
                    match error {
                        RelayError::ConnectionError => {
                            inc_conn_relay_errors(relay);
                        }
                        RelayError::TooManyRequests => {
                            inc_too_many_req_relay_errors(relay);
                        }
                        _ => {
                            inc_other_relay_errors(relay);
                        }
                    };
                    continue;
                }
            };

            let min_slot = registrations.iter().map(|v| v.slot).min().unwrap_or(0);
            let max_slot = registrations.iter().map(|v| v.slot).max().unwrap_or(0);
            let mut this = self.0.write();
            let current_registrations = this.entry(client.id().clone()).or_default();

            // Remove old registrations and update the new ones.
            current_registrations.retain(|slot, _| slot >= &min_slot);
            current_registrations.extend(registrations.into_iter().map(|r| (r.slot, r)));

            let len = current_registrations.len();
            info!(relay = %client.id(), len, min_slot, max_slot, "Updated validator registrations");
        }
    }

    fn get_slot_registrations(&self, slot: u64) -> Vec<(MevBoostRelayID, ValidatorSlotData)> {
        let mut slot_registrations = Vec::new();
        for (relay_id, registrations) in self.0.read().iter() {
            if let Some(slot_registration) = registrations.get(&slot) {
                slot_registrations.push((relay_id.clone(), slot_registration.clone()));
            }
        }
        slot_registrations
    }
}

/// Helper to get SlotData from all relays.
#[derive(Debug)]
pub struct RelaysForSlotData {
    /// Validator registration cache.
    cache: RelayValidatorSlotDataCache,
    /// Redundant with relay but easier to access/pass around.
    can_ignore_gas_limit: HashSet<MevBoostRelayID>,
}

impl RelaysForSlotData {
    pub fn spawn_with_interval(
        relays: Vec<MevBoostRelaySlotInfoProvider>,
        interval: Duration,
        cancellation_token: CancellationToken,
    ) -> Self {
        let cache = RelayValidatorSlotDataCache::default();
        let can_ignore_gas_limit = relays
            .iter()
            .filter(|relay| relay.can_ignore_gas_limit())
            .map(|relay| relay.id().clone())
            .collect();

        tokio::spawn(Box::pin({
            let cache = cache.clone();
            async move {
                loop {
                    if timeout(interval, cancellation_token.cancelled())
                        .await
                        .is_ok()
                    {
                        return;
                    }
                    cache.update(&relays).await;
                }
            }
        }));

        Self {
            cache,
            can_ignore_gas_limit,
        }
    }

    /// Asks all relays in parallel for ValidatorSlotData.
    /// Under inconsistencies, the first one (the one with the highest priority as sorted on new) wins and any relay giving a different data
    /// is not included on the result.
    pub fn slot_data(
        &mut self,
        slot: u64,
    ) -> Option<(SlotData, Arc<HashMap<MevBoostRelayID, ValidatorSlotData>>)> {
        let registrations = self.cache.get_slot_registrations(slot);
        resolve_relay_slot_data(registrations, &self.can_ignore_gas_limit)
    }
}

fn resolve_relay_slot_data(
    fetched_data: Vec<(MevBoostRelayID, ValidatorSlotData)>,
    can_ignore_gas_limit: &HashSet<MevBoostRelayID>,
) -> Option<(SlotData, Arc<HashMap<MevBoostRelayID, ValidatorSlotData>>)> {
    if fetched_data.is_empty() {
        return None;
    }

    let mut registrations_by_slot_data: HashMap<
        SlotData,
        HashMap<MevBoostRelayID, ValidatorSlotData>,
    > = HashMap::default();

    for (relay, raw_data) in fetched_data {
        let slot_data = SlotData {
            fee_recipient: raw_data.entry.message.fee_recipient,
            gas_limit: raw_data.entry.message.gas_limit,
            pubkey: raw_data.entry.message.pubkey,
        };
        registrations_by_slot_data
            .entry(slot_data)
            .or_default()
            .insert(relay, raw_data);
    }

    // all relays returned the same data
    if registrations_by_slot_data.len() == 1 {
        let (slot_data, relay_registrations) =
            registrations_by_slot_data.into_iter().next().unwrap();
        return Some((slot_data, Arc::new(relay_registrations)));
    }

    let (latest_slot_data, mut selected_registrations) = registrations_by_slot_data
        .iter()
        .max_by_key(|(_, v)| {
            v.iter()
                .map(|(_, r)| r.entry.message.timestamp)
                .max()
                .unwrap_or_default()
        })
        .map(|(slot_data, registrations)| (slot_data.clone(), registrations.clone()))
        .unwrap();
    info!(?latest_slot_data, ?selected_registrations, all_registrations = ?registrations_by_slot_data, "Relays returned different slot data");

    // Add all relays that can ignore gas limit to the selected relays.
    for (slot, registrations) in registrations_by_slot_data {
        if slot.fee_recipient == latest_slot_data.fee_recipient
            && slot.pubkey == latest_slot_data.pubkey
            && slot.gas_limit != latest_slot_data.gas_limit
        {
            for (relay, registration) in registrations {
                if can_ignore_gas_limit.contains(&relay) {
                    info!(?latest_slot_data, %relay, "Upgraded relay set with can_ignore_gas_limit relay");
                    selected_registrations.insert(relay, registration);
                }
            }
        }
    }

    Some((latest_slot_data, Arc::new(selected_registrations)))
}

#[cfg(test)]
mod test {
    use crate::{
        mev_boost::{ValidatorRegistration, ValidatorRegistrationMessage},
        utils::set_test_debug_tracing_subscriber,
    };

    use super::*;
    use alloy_primitives::{address, Bytes};

    fn make_test_data(fee_recipient: Address, timestamp: u64) -> ValidatorSlotData {
        ValidatorSlotData {
            entry: ValidatorRegistration {
                message: ValidatorRegistrationMessage {
                    fee_recipient,
                    gas_limit: 30000000,
                    timestamp,
                    pubkey: H384::zero(),
                },
                signature: Bytes::new(),
            },
            validator_index: 1,
            slot: 2,
            regional_endpoints: Vec::new(),
        }
    }

    #[test]
    fn test_resolve_slot_data() {
        set_test_debug_tracing_subscriber();

        // Test when all relays return same data
        let relay1 = MevBoostRelayID::from("relay1");
        let relay2 = MevBoostRelayID::from("relay2");
        let relay3 = MevBoostRelayID::from("relay3");
        let data = make_test_data(address!("1111111111111111111111111111111111111111"), 100);

        let fetched = vec![
            (relay1.clone(), data.clone()),
            (relay2.clone(), data.clone()),
        ];

        let result = resolve_relay_slot_data(fetched.clone(), &HashSet::default());
        let (slot_data, relays) = result.unwrap();
        assert_eq!(
            SlotData {
                fee_recipient: address!("1111111111111111111111111111111111111111"),
                gas_limit: 30000000,
                pubkey: H384::zero(),
            },
            slot_data
        );
        assert_eq!(relays, Arc::new(HashMap::from_iter(fetched)));

        // Test when relays return different data (should pick latest timestamp)
        let data2 = make_test_data(address!("2222222222222222222222222222222222222222"), 200);
        let fetched = vec![
            (relay1.clone(), data.clone()),
            (relay2.clone(), data2.clone()),
        ];

        let result = resolve_relay_slot_data(fetched, &HashSet::default());
        let (slot_data, relays) = result.unwrap();
        assert_eq!(
            SlotData {
                fee_recipient: address!("2222222222222222222222222222222222222222"),
                gas_limit: 30000000,
                pubkey: H384::zero(),
            },
            slot_data
        );
        assert_eq!(
            relays,
            Arc::new(HashMap::from_iter([(relay2.clone(), data2.clone())]))
        );

        // Test when relays return different gas limit but same fee recipient and pubkey
        let mut data3 = data2.clone();
        data3.entry.message.gas_limit = 40000000;
        // We want data2 to win.
        data3.entry.message.timestamp -= 1;
        let fetched = vec![
            (relay1.clone(), data.clone()),
            (relay2.clone(), data2.clone()),
            (relay3.clone(), data3.clone()),
        ];

        // No can_ignore_gas_limit
        let result = resolve_relay_slot_data(fetched, &HashSet::default());
        let (slot_data, relays) = result.unwrap();
        assert_eq!(
            SlotData {
                fee_recipient: address!("2222222222222222222222222222222222222222"),
                gas_limit: 30000000,
                pubkey: H384::zero(),
            },
            slot_data
        );
        assert_eq!(
            relays,
            Arc::new(HashMap::from_iter([(relay2.clone(), data2.clone())]))
        );

        // data3 can_ignore_gas_limit
        let fetched = vec![
            (relay1.clone(), data.clone()),
            (relay2.clone(), data2.clone()),
            (relay3.clone(), data3.clone()),
        ];
        let can_ignore_gas_limit = HashSet::from_iter([relay3.clone()]);
        let result = resolve_relay_slot_data(fetched, &can_ignore_gas_limit);
        let (slot_data, relays) = result.unwrap();
        assert_eq!(
            SlotData {
                fee_recipient: address!("2222222222222222222222222222222222222222"),
                gas_limit: 30000000,
                pubkey: H384::zero(),
            },
            slot_data
        );
        assert_eq!(
            relays,
            Arc::new(HashMap::from_iter([
                (relay2.clone(), data2.clone()),
                (relay3.clone(), data3.clone()),
            ]))
        );
    }
}
