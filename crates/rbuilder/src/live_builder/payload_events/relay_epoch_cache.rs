use crate::{
    mev_boost::{RelayError, ValidatorSlotData},
    primitives::mev_boost::{MevBoostRelay, MevBoostRelayID},
    telemetry::{inc_conn_relay_errors, inc_other_relay_errors, inc_too_many_req_relay_errors},
};
use alloy_primitives::Address;
use futures::stream::FuturesOrdered;
use primitive_types::H384;
use tokio_stream::StreamExt;
use tracing::{info_span, trace, warn};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct SlotData {
    pub fee_recipient: Address,
    pub gas_limit: u64,
    pub pubkey: H384,
}

#[derive(Debug)]
struct RelayEpochCache {
    relay: MevBoostRelay,
    min_slot: u64,
    max_slot: u64,
    slot_data: Vec<ValidatorSlotData>,
}

impl RelayEpochCache {
    fn new(relay: MevBoostRelay) -> Self {
        Self {
            relay,
            min_slot: 0,
            max_slot: 0,
            slot_data: Vec::new(),
        }
    }

    async fn update_epoch_data(&mut self) -> Result<(), RelayError> {
        // @Far validate signatures of proposers here to make sure that relay is correct.
        let validators = self.relay.client.get_current_epoch_validators().await?;
        let min_slot = validators.iter().map(|v| v.slot).min().unwrap_or(0);
        let max_slot = validators.iter().map(|v| v.slot).max().unwrap_or(0);

        self.slot_data = validators;
        self.min_slot = min_slot;
        self.max_slot = max_slot;

        Ok(())
    }

    async fn get_slot_data(&mut self, slot: u64) -> Result<Option<ValidatorSlotData>, RelayError> {
        if slot < self.min_slot || slot > self.max_slot {
            self.update_epoch_data().await?;
        }

        Ok(self.slot_data.iter().find(|v| v.slot == slot).cloned())
    }
}

#[derive(Debug)]
pub struct RelaysForSlotData {
    relay: Vec<(MevBoostRelayID, RelayEpochCache)>,
}

impl RelaysForSlotData {
    pub fn new(relays: &[MevBoostRelay]) -> Self {
        // we sort relays so the relay with the highest priority will determine what is "correct" version of the epoch data.
        let sorted_relays = {
            let mut relays = relays.to_vec();
            relays.sort_by_key(|r| r.priority);
            relays
        };
        Self {
            relay: sorted_relays
                .into_iter()
                .map(|relay| (relay.id.clone(), RelayEpochCache::new(relay.clone())))
                .collect(),
        }
    }

    pub async fn slot_data(&mut self, slot: u64) -> Option<(SlotData, Vec<MevBoostRelayID>)> {
        // ask all relays concurrently about the slot
        let relay_res = self
            .relay
            .iter_mut()
            .map(|(k, v)| async { (k.clone(), v.get_slot_data(slot).await) })
            .collect::<FuturesOrdered<_>>()
            .collect::<Vec<_>>()
            .await;

        let mut slot_data = None;
        let mut relays = Vec::new();
        for (relay, res) in relay_res {
            let span = info_span!("relay", relay, slot);
            let _span_guard = span.enter();
            let relay_data = match res {
                Ok(Some(res)) => {
                    trace!("Got slot data from the relay");
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
            let relay_slot_data = SlotData {
                fee_recipient: relay_data.entry.message.fee_recipient,
                gas_limit: relay_data.entry.message.gas_limit,
                pubkey: relay_data.entry.message.pubkey,
            };
            if let Some(slot_data) = &slot_data {
                if slot_data != &relay_slot_data {
                    warn!(
                        relay_slot_data = ?relay_slot_data, slot_data = ?slot_data,
                        "Relay returned slot data that is different from returned from other relay",
                    );
                    continue;
                }
            } else {
                // since relays are sorted the relay with the highest priority will determine the value of slot_data
                slot_data = Some(relay_slot_data);
            }
            relays.push(relay);
        }
        slot_data.map(|d| (d, relays))
    }
}
