use crate::{
    mev_boost::{RelayError, ValidatorSlotData},
    primitives::mev_boost::{MevBoostRelayID, MevBoostRelaySlotInfoProvider},
    telemetry::{inc_conn_relay_errors, inc_other_relay_errors, inc_too_many_req_relay_errors},
};
use ahash::HashMap;
use alloy_primitives::Address;
use futures::stream::FuturesOrdered;
use primitive_types::H384;
use tokio_stream::StreamExt;
use tracing::{info, info_span, trace, trace_span, warn};

/// Info about a slot obtained from a relay.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Default)]
pub struct SlotData {
    /// fee recipient the validator chose.
    pub fee_recipient: Address,
    pub gas_limit: u64,
    /// Selected registered validator for the slot key.
    pub pubkey: H384,
}

/// Gets ValidatorSlotData for a single slot via get_slot_data.
/// Since the low level API used (/relay/v1/builder/validators) brings current and next epoch validator data it caches the results.
#[derive(Debug)]
struct RelayEpochCache {
    relay: MevBoostRelaySlotInfoProvider,
    min_slot: u64,
    max_slot: u64,
    slot_data: Vec<ValidatorSlotData>,
}

impl RelayEpochCache {
    fn new(relay: MevBoostRelaySlotInfoProvider) -> Self {
        Self {
            relay,
            min_slot: 0,
            max_slot: 0,
            slot_data: Vec::new(),
        }
    }

    async fn update_epoch_data(&mut self) -> Result<(), RelayError> {
        // @Far validate signatures of proposers here to make sure that relay is correct.
        let validators = self.relay.get_current_epoch_validators().await?;
        let min_slot = validators.iter().map(|v| v.slot).min().unwrap_or(0);
        let max_slot = validators.iter().map(|v| v.slot).max().unwrap_or(0);

        self.slot_data = validators;
        self.min_slot = min_slot;
        self.max_slot = max_slot;

        Ok(())
    }

    /// Might fail (None) if the slot is in the past or far in the future.
    /// Ideally, it's called just for the next slot.
    async fn get_slot_data(&mut self, slot: u64) -> Result<Option<ValidatorSlotData>, RelayError> {
        if slot < self.min_slot || slot > self.max_slot {
            self.update_epoch_data().await?;
        }

        Ok(self.slot_data.iter().find(|v| v.slot == slot).cloned())
    }
}

/// Helper to get SlotData from all relays.
#[derive(Debug)]
pub struct RelaysForSlotData {
    /// Sorted by priority so when we use them on slot_data the one with the highest priority wins.
    relay: Vec<(MevBoostRelayID, RelayEpochCache)>,
}

impl RelaysForSlotData {
    pub fn new(relays: &[MevBoostRelaySlotInfoProvider]) -> Self {
        Self {
            relay: relays
                .iter()
                .map(|relay| (relay.id().clone(), RelayEpochCache::new(relay.clone())))
                .collect(),
        }
    }

    /// Asks all relays in parallel for ValidatorSlotData.
    /// Under unconsistencies, the first one (the one with the highest priority as sorted on new) wins and any relay giving a different data
    /// is not included on the result.
    pub async fn slot_data(&mut self, slot: u64) -> Option<(SlotData, Vec<MevBoostRelayID>)> {
        // ask all relays concurrently about the slot
        let relay_res = self
            .relay
            .iter_mut()
            .map(|(k, v)| async { (k.clone(), v.get_slot_data(slot).await) })
            .collect::<FuturesOrdered<_>>()
            .collect::<Vec<_>>()
            .await;

        let mut relay_ok_res = Vec::new();
        for (relay, res) in relay_res {
            let span = info_span!("relay", relay, slot);
            let _span_guard = span.enter();
            let relay_data = match res {
                Ok(Some(res)) => {
                    trace!(?res, "Got slot data from the relay");
                    res
                }
                Ok(None) => {
                    trace!("Relay does not have slot data");
                    continue;
                }
                Err(err) => {
                    match err {
                        RelayError::ConnectionError => {
                            inc_conn_relay_errors(&relay);
                        }
                        RelayError::TooManyRequests => {
                            inc_too_many_req_relay_errors(&relay);
                        }
                        _ => {
                            inc_other_relay_errors(&relay);
                        }
                    }
                    // we always warn here because error at this stage => no bids for slot on this relay
                    warn!(err = ?err,"Relay returned error while getting epoch data, error");
                    continue;
                }
            };
            assert_eq!(relay_data.slot, slot);
            relay_ok_res.push((relay, relay_data));
        }
        resolve_relay_slot_data(relay_ok_res)
    }
}

fn resolve_relay_slot_data(
    fetched_data: Vec<(MevBoostRelayID, ValidatorSlotData)>,
) -> Option<(SlotData, Vec<MevBoostRelayID>)> {
    if fetched_data.is_empty() {
        return None;
    }

    let mut slot_relays: HashMap<SlotData, Vec<MevBoostRelayID>> = HashMap::default();
    let mut slot_raw_data: HashMap<SlotData, Vec<ValidatorSlotData>> = HashMap::default();

    for (relay, raw_data) in fetched_data {
        let slot_data = SlotData {
            fee_recipient: raw_data.entry.message.fee_recipient,
            gas_limit: raw_data.entry.message.gas_limit,
            pubkey: raw_data.entry.message.pubkey,
        };
        slot_relays
            .entry(slot_data.clone())
            .or_default()
            .push(relay);
        slot_raw_data.entry(slot_data).or_default().push(raw_data);
    }

    // all relays returned the same data
    if slot_relays.len() == 1 {
        let (slot_data, relays) = slot_relays.into_iter().next().unwrap();
        return Some((slot_data, relays));
    }

    let (latest_slot_data, _) = slot_raw_data
        .iter()
        .max_by_key(|(_, v)| {
            v.iter()
                .map(|r| r.entry.message.timestamp)
                .max()
                .unwrap_or_default()
        })
        .unwrap();
    let selected_relays = slot_relays.get(latest_slot_data).unwrap();

    let span = trace_span!("raw_relay_data", ?slot_raw_data);
    let _span_guard = span.enter();
    info!(all_data = ?slot_relays, ?selected_relays, "Relays returned different slot data");

    Some(slot_relays.remove_entry(latest_slot_data).unwrap())
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
        }
    }

    #[test]
    fn test_resolve_slot_data() {
        set_test_debug_tracing_subscriber();

        // Test when all relays return same data
        let relay1 = MevBoostRelayID::from("relay1");
        let relay2 = MevBoostRelayID::from("relay2");
        let data = make_test_data(address!("1111111111111111111111111111111111111111"), 100);

        let fetched = vec![
            (relay1.clone(), data.clone()),
            (relay2.clone(), data.clone()),
        ];

        let result = resolve_relay_slot_data(fetched);
        let (slot_data, relays) = result.unwrap();
        assert_eq!(
            SlotData {
                fee_recipient: address!("1111111111111111111111111111111111111111"),
                gas_limit: 30000000,
                pubkey: H384::zero(),
            },
            slot_data
        );
        assert_eq!(relays, vec![relay1.clone(), relay2.clone()]);

        // Test when relays return different data (should pick latest timestamp)
        let data2 = make_test_data(address!("2222222222222222222222222222222222222222"), 200);
        let fetched = vec![
            (relay1.clone(), data.clone()),
            (relay2.clone(), data2.clone()),
        ];

        let result = resolve_relay_slot_data(fetched);
        let (slot_data, relays) = result.unwrap();
        assert_eq!(
            SlotData {
                fee_recipient: address!("2222222222222222222222222222222222222222"),
                gas_limit: 30000000,
                pubkey: H384::zero(),
            },
            slot_data
        );
        assert_eq!(relays, vec![relay2.clone()]);
    }
}
