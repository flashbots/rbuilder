use alloy_primitives::B256;
use alloy_rpc_types_beacon::events::PayloadAttributesEvent;
use beacon_api_client::{mainnet::Client as bClient, Error, Topic};
use mev_share_sse::client::EventStream;
use rbuilder_primitives::epbs::{
    ExecutionPayloadEnvelope, SignedExecutionPayloadBid, SignedExecutionPayloadEnvelope,
    SignedProposerPreferences,
};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use std::{collections::HashMap, fmt::Debug};
use url::Url;

#[derive(Debug, Clone, Deserialize)]
pub struct GenesisData {
    #[serde(with = "serde_utils::quoted_u64")]
    pub genesis_time: u64,
    pub genesis_validators_root: B256,
    #[serde(with = "serde_utils::bytes_4_hex")]
    pub genesis_fork_version: [u8; 4],
}

#[derive(Debug, Clone, Deserialize)]
struct GenesisResponse {
    data: GenesisData,
}

/// Validator data from the beacon chain.
#[derive(Debug, Clone, Deserialize)]
pub struct ValidatorData {
    /// validator index
    #[serde(with = "serde_utils::quoted_u64")]
    pub index: u64,
    /// validators balance in gwei
    #[serde(with = "serde_utils::quoted_u64")]
    pub balance: u64,
    /// validators status
    pub status: String,
    /// validator details
    pub validator: ValidatorDetails,
}

/// Detailed validator information.
#[derive(Debug, Clone, Deserialize)]
pub struct ValidatorDetails {
    pub pubkey: String,
    pub withdrawal_credentials: String,
    #[serde(with = "serde_utils::quoted_u64")]
    pub effective_balance: u64,
    pub slashed: bool,
    #[serde(with = "serde_utils::quoted_u64")]
    pub activation_eligibility_epoch: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub activation_epoch: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub exit_epoch: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub withdrawable_epoch: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct ValidatorResponse {
    data: ValidatorData,
}

mod serde_utils {
    use serde::{Deserialize, Deserializer};

    pub mod quoted_u64 {
        use super::*;

        pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
        where
            D: Deserializer<'de>,
        {
            let s = String::deserialize(deserializer)?;
            s.parse().map_err(serde::de::Error::custom)
        }
    }

    pub mod bytes_4_hex {
        use super::*;

        pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 4], D::Error>
        where
            D: Deserializer<'de>,
        {
            let s = String::deserialize(deserializer)?;
            let s = s.strip_prefix("0x").unwrap_or(&s);
            let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;
            if bytes.len() != 4 {
                return Err(serde::de::Error::custom("expected 4 bytes"));
            }
            let mut arr = [0u8; 4];
            arr.copy_from_slice(&bytes);
            Ok(arr)
        }
    }
}

pub const DEFAULT_CL_NODE_URL: &str = "http://localhost:8000";

#[derive(Deserialize, Clone)]
#[serde(try_from = "String")]
pub struct Client {
    inner: bClient,
    endpoint_url: Url,
}

impl Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish()
    }
}

impl Default for Client {
    fn default() -> Self {
        let url = Url::parse(DEFAULT_CL_NODE_URL).unwrap();
        Self {
            inner: bClient::new(url.clone()),
            endpoint_url: url,
        }
    }
}

impl Client {
    pub fn new(endpoint: Url) -> Self {
        Self {
            inner: bClient::new(endpoint.clone()),
            endpoint_url: endpoint,
        }
    }

    pub fn endpoint(&self) -> &Url {
        &self.endpoint_url
    }

    pub async fn get_spec(&self) -> Result<HashMap<String, String>, Error> {
        self.inner.get_spec().await
    }

    pub async fn get_seconds_per_slot(&self) -> eyre::Result<u64> {
        let url = self
            .endpoint_url
            .join("eth/v1/config/spec")
            .map_err(|e| eyre::eyre!("Invalid URL: {}", e))?;

        let response = reqwest::Client::new()
            .get(url)
            .header("Accept", "application/json")
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Failed to fetch chain spec: {} - {}",
                status,
                body
            ));
        }

        let body: serde_json::Value = response.json().await?;
        let raw = body
            .get("data")
            .and_then(|d| d.get("SECONDS_PER_SLOT"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("Chain spec response missing data.SECONDS_PER_SLOT"))?;
        raw.parse::<u64>()
            .map_err(|e| eyre::eyre!("Failed to parse SECONDS_PER_SLOT='{}': {}", raw, e))
    }

    pub async fn get_events<T: Topic>(&self) -> Result<EventStream<T::Data>, Error> {
        self.inner.get_events::<T>().await
    }

    /// Fetch genesis data from the beacon chain.
    /// Returns the genesis time, genesis validators root, and genesis fork version.
    pub async fn get_genesis(&self) -> eyre::Result<GenesisData> {
        let url = self
            .endpoint_url
            .join("eth/v1/beacon/genesis")
            .map_err(|e| eyre::eyre!("Invalid URL: {}", e))?;

        let response = reqwest::get(url).await?;

        if !response.status().is_success() {
            return Err(eyre::eyre!("Failed to get genesis: {}", response.status()));
        }

        let genesis_response: GenesisResponse = response.json().await?;

        Ok(genesis_response.data)
    }

    /// Fetch the active fork version at head
    pub async fn get_head_fork_version(&self) -> eyre::Result<[u8; 4]> {
        let url = self
            .endpoint_url
            .join("eth/v1/beacon/states/head/fork")
            .map_err(|e| eyre::eyre!("Invalid URL: {}", e))?;

        let response = reqwest::get(url).await?;
        if !response.status().is_success() {
            return Err(eyre::eyre!("Failed to get fork: {}", response.status()));
        }

        let val: serde_json::Value = response.json().await?;
        let current_version = val
            .get("data")
            .and_then(|d| d.get("current_version"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("No current_version in fork response"))?;

        let s = current_version
            .strip_prefix("0x")
            .unwrap_or(current_version);
        let bytes = hex::decode(s).map_err(|e| eyre::eyre!("Invalid fork hex: {}", e))?;
        if bytes.len() != 4 {
            return Err(eyre::eyre!(
                "Expected 4-byte fork version, got {}",
                bytes.len()
            ));
        }
        let mut arr = [0u8; 4];
        arr.copy_from_slice(&bytes);
        Ok(arr)
    }

    /// Fetch validator data from the beacon chain by pubkey or index.
    ///
    /// The `validator_id` can be either:
    /// - A hex encoded BLS public key
    /// - A validator index as a string
    pub async fn get_validator(&self, validator_id: &str) -> eyre::Result<ValidatorData> {
        let path = format!("eth/v1/beacon/states/head/validators/{}", validator_id);
        let url = self
            .endpoint_url
            .join(&path)
            .map_err(|e| eyre::eyre!("Invalid URL: {}", e))?;

        let response = reqwest::get(url).await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(eyre::eyre!(
                "Validator not found: {}. Make sure the builder validator is registered and active on the beacon chain.",
                validator_id
            ));
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Failed to get validator {}: {} - {}",
                validator_id,
                status,
                body
            ));
        }

        let validator_response: ValidatorResponse = response.json().await?;

        Ok(validator_response.data)
    }

    /// Fetch validator data from the beacon chain by BLS public key.
    /// jut a helper method that formats the public key correctly.
    pub async fn get_validator_by_pubkey(&self, pubkey: &[u8]) -> eyre::Result<ValidatorData> {
        let pubkey_hex = format!("0x{}", hex::encode(pubkey));
        self.get_validator(&pubkey_hex).await
    }

    /// Fetch a builder's index and `deposit_epoch` from the beacon state by BLS
    /// public key.
    pub async fn get_builder_entry_by_pubkey(&self, pubkey: &[u8]) -> eyre::Result<(u64, u64)> {
        let pubkey_hex = format!("0x{}", hex::encode(pubkey));

        let url = self
            .endpoint_url
            .join("eth/v2/debug/beacon/states/head")
            .map_err(|e| eyre::eyre!("Invalid URL: {}", e))?;

        let response = reqwest::Client::new()
            .get(url)
            .header("Accept", "application/json")
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Failed to fetch beacon state: {} - {}",
                status,
                body
            ));
        }

        // parse only the builders field from the state to avoid deserializing everything
        let state_response: serde_json::Value = response.json().await?;
        let builders = state_response
            .get("data")
            .and_then(|d| d.get("builders"))
            .and_then(|b| b.as_array())
            .ok_or_else(|| {
                eyre::eyre!(
                    "Beacon state does not contain builders field. \
                     Is the chain at Gloas fork yet?"
                )
            })?;

        for (index, builder) in builders.iter().enumerate() {
            let pk = match builder.get("pubkey").and_then(|p| p.as_str()) {
                Some(pk) => pk,
                None => continue,
            };
            if pk != pubkey_hex {
                continue;
            }
            let deposit_epoch_str = builder
                .get("deposit_epoch")
                .and_then(|d| d.as_str())
                .ok_or_else(|| {
                    eyre::eyre!("Builder entry at index {} is missing deposit_epoch", index)
                })?;
            let deposit_epoch: u64 = deposit_epoch_str.parse().map_err(|e| {
                eyre::eyre!(
                    "Failed to parse deposit_epoch '{}': {}",
                    deposit_epoch_str,
                    e
                )
            })?;
            return Ok((index as u64, deposit_epoch));
        }

        Err(eyre::eyre!(
            "Builder with pubkey {} not found in beacon state builders registry. \
             Make sure the builder has been deposited with BUILDER_WITHDRAWAL_PREFIX (0x03).",
            pubkey_hex
        ))
    }

    /// Fetch the current finalized epoch
    pub async fn get_finalized_epoch(&self) -> eyre::Result<u64> {
        let url = self
            .endpoint_url
            .join("eth/v1/beacon/states/head/finality_checkpoints")
            .map_err(|e| eyre::eyre!("Invalid URL: {}", e))?;

        let response = reqwest::Client::new()
            .get(url)
            .header("Accept", "application/json")
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Failed to fetch finality checkpoints: {} - {}",
                status,
                body
            ));
        }

        let body: serde_json::Value = response.json().await?;
        let epoch_str = body
            .get("data")
            .and_then(|d| d.get("finalized"))
            .and_then(|f| f.get("epoch"))
            .and_then(|e| e.as_str())
            .ok_or_else(|| {
                eyre::eyre!("finality_checkpoints response missing data.finalized.epoch")
            })?;
        epoch_str
            .parse::<u64>()
            .map_err(|e| eyre::eyre!("Failed to parse finalized epoch '{}': {}", epoch_str, e))
    }

    /// Submit a signed execution payload bid to p2p via the beacon node.
    pub async fn submit_execution_payload_bid(
        &self,
        bid: &SignedExecutionPayloadBid,
    ) -> eyre::Result<()> {
        let url = self
            .endpoint_url
            .join("eth/v1/beacon/execution_payload_bid")
            .map_err(|e| eyre::eyre!("Invalid URL: {}", e))?;

        let response = reqwest::Client::new()
            .post(url)
            .header("Eth-Consensus-Version", "gloas")
            .json(bid)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Failed to submit execution payload bid: {} - {}",
                status,
                body
            ));
        }

        Ok(())
    }

    /// Submit a signed execution payload envelope to p2p via the beacon node.
    pub async fn submit_execution_payload_envelope(
        &self,
        envelope: &SignedExecutionPayloadEnvelope,
        blobs: &[alloy_primitives::Bytes],
        cell_proofs: &[alloy_primitives::Bytes],
    ) -> eyre::Result<()> {
        let url = self
            .endpoint_url
            .join("eth/v1/beacon/execution_payload_envelope")
            .map_err(|e| eyre::eyre!("Invalid URL: {}", e))?;

        let body = PublishEnvelopeRequest {
            message: &envelope.message,
            signature: &envelope.signature,
            blobs: blobs.iter().map(hex_encode).collect(),
            cell_proofs: cell_proofs.iter().map(hex_encode).collect(),
        };

        // Bound the envelope reveal POST so a stuck cl cannot leak our spawned
        // reveal task indefinitely.
        // TODO: revisit the timeout value
        const ENVELOPE_REVEAL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(5500);
        let response = reqwest::Client::new()
            .post(url)
            .header("Eth-Consensus-Version", "gloas")
            .json(&body)
            .timeout(ENVELOPE_REVEAL_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    eyre::eyre!(
                        "Envelope reveal post timed out after {:?}",
                        ENVELOPE_REVEAL_TIMEOUT
                    )
                } else {
                    eyre::eyre!("Envelope reveal POST failed: {}", e)
                }
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Failed to submit execution payload envelope: {} - {}",
                status,
                body
            ));
        }

        Ok(())
    }

    /// Fetch a beacon blocks `signed_execution_payload_bid` and `parent_root`.
    ///
    /// used at reveal time:
    ///   * the bid tells us which block_hash the proposer committed to (so we
    ///     can look up the matching cached payload) and
    ///   * `parent_root` is what the envelope's `parent_beacon_block_root`
    ///     field must be set to
    pub async fn get_beacon_block_bid(
        &self,
        block_id: &str,
    ) -> eyre::Result<Option<BeaconBlockBidInfo>> {
        let path = format!("eth/v2/beacon/blocks/{}", block_id);
        let url = self
            .endpoint_url
            .join(&path)
            .map_err(|e| eyre::eyre!("Invalid URL: {}", e))?;

        let response = reqwest::get(url).await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Failed to get beacon block {}: {} - {}",
                block_id,
                status,
                body
            ));
        }

        let block_response: serde_json::Value = response.json().await?;
        let message = block_response
            .get("data")
            .and_then(|d| d.get("message"))
            .ok_or_else(|| eyre::eyre!("Missing data.message in beacon block response"))?;

        let parent_root = message
            .get("parent_root")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("Missing parent_root in beacon block message"))?;
        let parent_root: B256 = parent_root
            .parse()
            .map_err(|e| eyre::eyre!("Invalid parent_root hex: {}", e))?;

        let bid = message
            .get("body")
            .and_then(|b| b.get("signed_execution_payload_bid"))
            .map(|bid_value| serde_json::from_value::<SignedExecutionPayloadBid>(bid_value.clone()))
            .transpose()
            .map_err(|e| eyre::eyre!("Failed to parse bid from block: {}", e))?;

        Ok(bid.map(|bid| BeaconBlockBidInfo { bid, parent_root }))
    }
}

#[derive(Debug, Clone)]
pub struct BeaconBlockBidInfo {
    pub bid: SignedExecutionPayloadBid,
    pub parent_root: B256,
}

impl TryFrom<String> for Client {
    type Error = url::ParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        let url = Url::parse(&s)?;
        Ok(Self {
            inner: bClient::new(url.clone()),
            endpoint_url: url,
        })
    }
}

pub struct PayloadAttributesTopic;

impl Topic for PayloadAttributesTopic {
    const NAME: &'static str = "payload_attributes";

    type Data = PayloadAttributesEvent;
}

/// SSE topic for head events from the beacon node.
pub struct HeadTopic;

impl Topic for HeadTopic {
    const NAME: &'static str = "head";

    type Data = HeadEvent;
}

/// SSE topic for execution payload bid events.
pub struct ExecutionPayloadBidTopic;

impl Topic for ExecutionPayloadBidTopic {
    const NAME: &'static str = "execution_payload_bid";

    type Data = SignedExecutionPayloadBid;
}

/// SSE topic for proposer preferences events.
pub struct ProposerPreferencesTopic;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ProposerPreferencesEvent {
    #[allow(dead_code)]
    pub version: String,
    pub data: SignedProposerPreferences,
}

impl Topic for ProposerPreferencesTopic {
    const NAME: &'static str = "proposer_preferences";

    type Data = ProposerPreferencesEvent;
}

/// Head event from the beacon node SSE stream.
#[serde_as]
#[derive(Debug, Clone, Deserialize)]
pub struct HeadEvent {
    #[serde_as(as = "DisplayFromStr")]
    pub slot: u64,
    pub block: B256,
    /// state root of the new head state.
    pub state: B256,
    #[serde(default)]
    pub execution_optimistic: bool,
}

/// Request body for POST /eth/v1/beacon/execution_payload_envelope.
/// TODO: verify the struct
#[derive(Debug, Clone, Serialize)]
struct PublishEnvelopeRequest<'a> {
    message: &'a ExecutionPayloadEnvelope,
    signature: &'a alloy_rpc_types_beacon::BlsSignature,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blobs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cell_proofs: Vec<String>,
}

fn hex_encode(b: &alloy_primitives::Bytes) -> String {
    format!("0x{}", hex::encode(b.as_ref()))
}

#[cfg(test)]
mod tests {
    // TODO: Enable these tests.
    use super::*;
    use futures::StreamExt;

    #[ignore]
    #[tokio::test]
    async fn test_get_spec() {
        let client = Client::default();
        let spec = client.get_spec().await.unwrap();

        // validate that the spec contains the genesis fork version
        spec.get("GENESIS_FORK_VERSION").unwrap();
    }

    #[ignore]
    #[tokio::test]
    async fn test_get_events() {
        let client = Client::default();
        let mut stream = client.get_events::<PayloadAttributesTopic>().await.unwrap();

        // validate that the stream is not empty
        // TODO: add timeout
        let event = stream.next().await.unwrap().unwrap();
        print!("{event:?}");
    }
}
