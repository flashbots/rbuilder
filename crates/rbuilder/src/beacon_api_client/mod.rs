use alloy_primitives::B256;
use alloy_rpc_types_beacon::events::PayloadAttributesEvent;
use beacon_api_client::{mainnet::Client as bClient, Error, Topic};
use mev_share_sse::client::EventStream;
use serde::Deserialize;
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
