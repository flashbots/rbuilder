use crate::mev_boost::{
    BuilderBlockReceived, MevBoostRelaySlotInfoProvider, RelayClient, RelayError,
};
use futures::{stream::FuturesUnordered, StreamExt};
use rbuilder_primitives::mev_boost::{MevBoostRelayID, RELAYS};
use std::collections::HashMap;
use tracing::trace;

#[derive(Debug)]
pub struct PayloadDeliveredResult {
    pub delivered: Vec<(MevBoostRelayID, BuilderBlockReceived)>,
    pub relay_errors: HashMap<MevBoostRelayID, DeliveredPayloadBidTraceErr>,
}

impl PayloadDeliveredResult {
    pub fn best_bid(&self) -> Option<BuilderBlockReceived> {
        self.delivered.first().map(|(_, p)| p.clone())
    }

    pub fn best_relay(&self) -> Option<MevBoostRelayID> {
        self.delivered.first().map(|(r, _)| r.clone())
    }
}

/// Struct that allows to get the delivered payloads for a block from several relays via RPC using [RelayClient]
#[derive(Debug, Clone)]
pub struct PayloadDeliveredFetcher {
    relays: HashMap<MevBoostRelayID, RelayClient>,
}

/// Uses some predefined relays ([RELAYS])
impl Default for PayloadDeliveredFetcher {
    fn default() -> Self {
        let relays = RELAYS
            .clone()
            .into_iter()
            .map(|r| {
                MevBoostRelaySlotInfoProvider::new(
                    RelayClient::from_known_relay(r.clone()),
                    r.name(),
                )
            })
            .collect::<Vec<_>>();

        Self::from_relays(&relays)
    }
}

impl PayloadDeliveredFetcher {
    pub fn from_relays(relays: &[MevBoostRelaySlotInfoProvider]) -> Self {
        let mut result = HashMap::new();
        for relay in relays {
            result.insert(relay.id().clone(), relay.client());
        }
        Self { relays: result }
    }

    /// Returns bid traces for the given block delivered by the selected relays
    /// sorted by timestamp_ms
    pub async fn get_payload_delivered(&self, block_number: u64) -> PayloadDeliveredResult {
        let mut relay_errors = HashMap::new();

        let delivered_payloads = self
            .relays
            .clone()
            .into_iter()
            .map(|(id, relay)| async move {
                (
                    id,
                    get_delivered_payload_bid_trace(relay, block_number).await,
                )
            })
            .collect::<FuturesUnordered<_>>()
            .collect::<Vec<_>>()
            .await;

        let mut relays_delivered_payload = Vec::new();
        for (relay, res) in delivered_payloads {
            match res {
                Ok(payload) => {
                    relays_delivered_payload.push((relay, payload));
                }
                Err(DeliveredPayloadBidTraceErr::NoDeliveredBidTrace) => {
                    trace!(
                        relay = relay,
                        "No payload bid trace for block {}",
                        block_number
                    );
                }
                Err(err) => {
                    trace!(
                        relay = relay,
                        "Relay error while delivering payload: {:?}",
                        err
                    );
                    relay_errors.insert(relay, err);
                }
            }
        }

        relays_delivered_payload.sort_by_key(|(_, p)| p.timestamp_ms);
        PayloadDeliveredResult {
            delivered: relays_delivered_payload,
            relay_errors,
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum DeliveredPayloadBidTraceErr {
    #[error("RelayError: {0}")]
    RelayError(#[from] RelayError),
    #[error("No delivered bid trace")]
    NoDeliveredBidTrace,
}

pub async fn get_delivered_payload_bid_trace(
    relay: RelayClient,
    block_number: u64,
) -> Result<BuilderBlockReceived, DeliveredPayloadBidTraceErr> {
    let payload = relay
        .proposer_payload_delivered_block_number(block_number)
        .await?
        .ok_or(DeliveredPayloadBidTraceErr::NoDeliveredBidTrace)?;

    let bid_trace = relay
        .builder_block_received_block_hash(payload.block_hash)
        .await?
        .ok_or(DeliveredPayloadBidTraceErr::NoDeliveredBidTrace)?;
    Ok(bid_trace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[ignore]
    #[tokio::test]
    async fn test_payload_delivered() {
        let fetcher = PayloadDeliveredFetcher::default();
        let res = fetcher.get_payload_delivered(19012899).await;
        dbg!(res);
    }
}
