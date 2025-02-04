mod error;
pub mod fake_mev_boost_relay;
pub mod rpc;
pub mod sign_payload;
pub mod submission;

use super::utils::u256decimal_serde_helper;

use alloy_primitives::{Address, BlockHash, Bytes, U256};
use flate2::{write::GzEncoder, Compression};
use primitive_types::H384;
use reqwest::{
    header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE},
    Body, Response, StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use ssz::Encode;
use std::{io::Write, str::FromStr};
use submission::{SubmitBlockRequest, SubmitBlockRequestNoBlobs, SubmitBlockRequestWithMetadata};
use url::Url;

pub use error::*;
pub use sign_payload::*;

const TOTAL_PAYMENT_HEADER: &str = "Total-Payment";
const TOP_BID_HEADER: &str = "Top-Bid";

const JSON_CONTENT_TYPE: &str = "application/json";
const SSZ_CONTENT_TYPE: &str = "application/octet-stream";
const GZIP_CONTENT_ENCODING: &str = "gzip";

const BUILDER_ID_HEADER: &str = "X-Builder-Id";
const API_TOKEN_HEADER: &str = "X-Api-Token";

/// We don't have nice error codes for relay errors so we have to parse looking for substrings :(
/// Simulation error base.
const SIM_FAILED_SUBSTRING: &str = "simulation failed";
/// Any error containing SIM_FAILED_SUBSTRING and any of SIM_FAILED_NON_CRITICAL_ERRORS is not critical so we should not stop block building.
const SIM_FAILED_NON_CRITICAL_ERRORS: &[&str] = &[
    "blacklisted address", // Generated block is good but contains a blacklisted address. This happens if we don't use an blacklist but the relay does.
    "unknown ancestor",
    "missing trie node",
];

// @Org consolidate with primitives::mev_boost

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum KnownRelay {
    Flashbots,
    BloxrouteMaxProfit,
    BloxrouteEthical,
    BloxrouteRegulated,
    Eden,
    SecureRpc,
    Ultrasound,
    Agnostic,
    Aestus,
    Wenmerge,
}

pub const RELAYS: [KnownRelay; 9] = [
    KnownRelay::Flashbots,
    KnownRelay::BloxrouteMaxProfit,
    KnownRelay::BloxrouteRegulated,
    KnownRelay::Eden,
    KnownRelay::SecureRpc,
    KnownRelay::Ultrasound,
    KnownRelay::Agnostic,
    KnownRelay::Aestus,
    KnownRelay::Wenmerge,
];

impl KnownRelay {
    pub fn url(&self) -> Url {
        Url::parse(match self {
            KnownRelay::Flashbots => "https://0xac6e77dfe25ecd6110b8e780608cce0dab71fdd5ebea22a16c0205200f2f8e2e3ad3b71d3499c54ad14d6c21b41a37ae@boost-relay.flashbots.net",
            KnownRelay::BloxrouteMaxProfit => "https://0x8b5d2e73e2a3a55c6c87b8b6eb92e0149a125c852751db1422fa951e42a09b82c142c3ea98d0d9930b056a3bc9896b8f@bloxroute.max-profit.blxrbdn.com",
            KnownRelay::BloxrouteEthical => "https://0xad0a8bb54565c2211cee576363f3a347089d2f07cf72679d16911d740262694cadb62d7fd7483f27afd714ca0f1b9118@bloxroute.ethical.blxrbdn.com",
            KnownRelay::BloxrouteRegulated => "https://0xb0b07cd0abef743db4260b0ed50619cf6ad4d82064cb4fbec9d3ec530f7c5e6793d9f286c4e082c0244ffb9f2658fe88@bloxroute.regulated.blxrbdn.com",
            KnownRelay::Eden => "https://0xb3ee7afcf27f1f1259ac1787876318c6584ee353097a50ed84f51a1f21a323b3736f271a895c7ce918c038e4265918be@relay.edennetwork.io",
            KnownRelay::SecureRpc => "https://0x98650451ba02064f7b000f5768cf0cf4d4e492317d82871bdc87ef841a0743f69f0f1eea11168503240ac35d101c9135@mainnet-relay.securerpc.com",
            KnownRelay::Ultrasound => "https://0xa1559ace749633b997cb3fdacffb890aeebdb0f5a3b6aaa7eeeaf1a38af0a8fe88b9e4b1f61f236d2e64d95733327a62@relay.ultrasound.money",
            KnownRelay::Agnostic => "https://0xa7ab7a996c8584251c8f925da3170bdfd6ebc75d50f5ddc4050a6fdc77f2a3b5fce2cc750d0865e05d7228af97d69561@agnostic-relay.net",
            KnownRelay::Aestus => "https://0xa15b52576bcbf1072f4a011c0f99f9fb6c66f3e1ff321f11f461d15e31b1cb359caa092c71bbded0bae5b5ea401aab7e@aestus.live",
            KnownRelay::Wenmerge => "https://0x8c7d33605ecef85403f8b7289c8058f440cbb6bf72b055dfe2f3e2c6695b6a1ea5a9cd0eb3a7982927a463feb4c3dae2@relay.wenmerge.com",
        }).unwrap()
    }

    pub fn name(&self) -> String {
        match self {
            KnownRelay::Flashbots => "flashbots",
            KnownRelay::BloxrouteMaxProfit => "bloxroute_max_profit",
            KnownRelay::BloxrouteEthical => "bloxroute_ethical",
            KnownRelay::BloxrouteRegulated => "bloxroute_regulated",
            KnownRelay::Eden => "eden",
            KnownRelay::SecureRpc => "secure_rpc",
            KnownRelay::Ultrasound => "ultrasound",
            KnownRelay::Agnostic => "agnostic",
            KnownRelay::Aestus => "aestus",
            KnownRelay::Wenmerge => "wenmerge",
        }
        .to_string()
    }
}

impl FromStr for KnownRelay {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "flashbots" => Ok(KnownRelay::Flashbots),
            "bloxroute_max_profit" => Ok(KnownRelay::BloxrouteMaxProfit),
            "bloxroute_ethical" => Ok(KnownRelay::BloxrouteEthical),
            "bloxroute_regulated" => Ok(KnownRelay::BloxrouteRegulated),
            "eden" => Ok(KnownRelay::Eden),
            "secure_rpc" => Ok(KnownRelay::SecureRpc),
            "ultrasound" => Ok(KnownRelay::Ultrasound),
            "agnostic" => Ok(KnownRelay::Agnostic),
            "aestus" => Ok(KnownRelay::Aestus),
            "wenmerge" => Ok(KnownRelay::Wenmerge),
            _ => Err(()),
        }
    }
}

/// Client to access part of relay APIs. See:
/// https://ethereum.github.io/builder-specs/#/Builder
/// https://flashbots.github.io/relay-specs/
#[derive(Debug, Clone)]
pub struct RelayClient {
    url: Url,
    client: reqwest::Client,
    authorization_header: Option<String>,
    builder_id_header: Option<String>,
    api_token_header: Option<String>,
}

impl RelayClient {
    pub fn from_url(
        url: Url,
        authorization_header: Option<String>,
        builder_id_header: Option<String>,
        api_token_header: Option<String>,
    ) -> Self {
        Self {
            url,
            client: reqwest::Client::new(),
            authorization_header,
            builder_id_header,
            api_token_header,
        }
    }

    pub fn from_known_relay(relay: KnownRelay) -> Self {
        Self::from_url(relay.url(), None, None, None)
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ProposerPayloadDelivered {
    #[serde_as(as = "DisplayFromStr")]
    pub slot: u64,
    pub parent_hash: BlockHash,
    pub block_hash: BlockHash,
    pub builder_pubkey: H384,
    pub proposer_pubkey: H384,
    pub proposer_fee_recipient: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub gas_limit: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub gas_used: u64,
    #[serde(with = "u256decimal_serde_helper")]
    pub value: U256,
    #[serde_as(as = "DisplayFromStr")]
    pub block_number: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub num_tx: u64,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct BuilderBlockReceived {
    #[serde_as(as = "DisplayFromStr")]
    pub slot: u64,
    pub parent_hash: BlockHash,
    pub block_hash: BlockHash,
    pub builder_pubkey: H384,
    pub proposer_pubkey: H384,
    pub proposer_fee_recipient: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub gas_limit: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub gas_used: u64,
    #[serde(with = "u256decimal_serde_helper")]
    pub value: U256,
    #[serde_as(as = "DisplayFromStr")]
    pub num_tx: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub block_number: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub timestamp: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub timestamp_ms: u64,
    #[serde(default)]
    pub optimistic_submission: bool,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Hash)]
pub struct ValidatorRegistrationMessage {
    pub fee_recipient: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub gas_limit: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub timestamp: u64,
    pub pubkey: H384,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Hash)]
pub struct ValidatorRegistration {
    pub message: ValidatorRegistrationMessage,
    pub signature: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RelayResponse<T> {
    Ok(T),
    Error(RedactableRelayErrorResponse),
}

/// Info about a registered validator selected as proposer for a slot.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Hash)]
pub struct ValidatorSlotData {
    #[serde_as(as = "DisplayFromStr")]
    pub slot: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub validator_index: u64,
    pub entry: ValidatorRegistration,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Too many txs")]
    TooManyTxs,
    #[error("Tx to big")]
    TxTooBig,
    #[error("Extra data too big")]
    ExtraDataTooBig,
    #[error("Too many withdrawals")]
    TooManyWithdrawals,
    #[error("Invalid signature")]
    InvalidSignature,
    #[error("Wrong KzgCommitment size")]
    WrongKzgCommitmentSize,
    #[error("Too many KzgCommitment")]
    TooManyKzgCommitments,
    #[error("Too many blobs")]
    TooManyBlobs,
    #[error("Blob to big")]
    BlobTooBig,
    #[error("Wrong proof size")]
    WrongProofSize,
    #[error("Too many proofs")]
    TooManyProofs,
}

#[derive(thiserror::Error)]
pub enum SubmitBlockErr {
    #[error("Relay error: {0}")]
    RelayError(#[from] RelayError),
    #[error("Payload attributes are not known")]
    PayloadAttributesNotKnown,
    #[error("Past slot")]
    PastSlot,
    #[error("Payload delivered")]
    PayloadDelivered,
    #[error("Bid below floor")]
    BidBelowFloor,
    #[cfg_attr(not(feature = "redact-sensitive"), error("Simulation Error: {0}"))]
    #[cfg_attr(feature = "redact-sensitive", error("Simulation Error: [REDACTED]"))]
    SimError(String),
    #[error("RPC conversion Error")]
    /// RPC validates the submissions (eg: limit of txs) much more that our model.
    RPCConversionError(Error),
    #[cfg_attr(
        not(feature = "redact-sensitive"),
        error("RPC serialization failed: {0}")
    )]
    #[cfg_attr(
        feature = "redact-sensitive",
        error("RPC serialization failed: [REDACTED]")
    )]
    RPCSerializationError(String),
    #[error("Invalid header")]
    InvalidHeader,
    #[error("Block known")]
    BlockKnown,
}

impl std::fmt::Debug for SubmitBlockErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self)
    }
}

// Data API
impl RelayClient {
    async fn get_one_delivered_payload(
        &self,
        query: &str,
    ) -> Result<Option<ProposerPayloadDelivered>, RelayError> {
        let url = {
            let mut url = self.url.clone();
            url.set_path("/relay/v1/data/bidtraces/proposer_payload_delivered");
            url.set_query(Some(query));
            url
        };

        let payloads = reqwest::get(url)
            .await?
            .json::<RelayResponse<Vec<ProposerPayloadDelivered>>>()
            .await?;

        match payloads {
            RelayResponse::Ok(payloads) => {
                if payloads.is_empty() {
                    return Ok(None);
                }
                Ok(Some(payloads[0].clone()))
            }
            RelayResponse::Error(error) => Err(RelayError::RelayError(error)),
        }
    }

    pub async fn proposer_payload_delivered_slot(
        &self,
        slot: u64,
    ) -> Result<Option<ProposerPayloadDelivered>, RelayError> {
        self.get_one_delivered_payload(&format!("slot={}", slot))
            .await
    }

    pub async fn proposer_payload_delivered_block_number(
        &self,
        block_number: u64,
    ) -> Result<Option<ProposerPayloadDelivered>, RelayError> {
        self.get_one_delivered_payload(&format!("block_number={}", block_number))
            .await
    }

    pub async fn proposer_payload_delivered_block_hash(
        &self,
        block_hash: BlockHash,
    ) -> Result<Option<ProposerPayloadDelivered>, RelayError> {
        self.get_one_delivered_payload(&format!("block_hash={:?}", block_hash))
            .await
    }

    async fn get_one_builder_block_received(
        &self,
        query: &str,
    ) -> Result<Option<BuilderBlockReceived>, RelayError> {
        let url = {
            let mut url = self.url.clone();
            url.set_path("/relay/v1/data/bidtraces/builder_blocks_received");
            url.set_query(Some(query));
            url
        };

        let payloads = reqwest::get(url)
            .await?
            .json::<RelayResponse<Vec<BuilderBlockReceived>>>()
            .await?;

        match payloads {
            RelayResponse::Ok(payloads) => {
                if payloads.is_empty() {
                    return Ok(None);
                }
                Ok(Some(payloads[0].clone()))
            }
            RelayResponse::Error(error) => Err(RelayError::RelayError(error)),
        }
    }

    pub async fn builder_block_received_block_hash(
        &self,
        block_hash: BlockHash,
    ) -> Result<Option<BuilderBlockReceived>, RelayError> {
        self.get_one_builder_block_received(&format!("block_hash={:?}", block_hash))
            .await
    }

    pub async fn validator_registration(
        &self,
        pubkey: H384,
    ) -> Result<Option<ValidatorRegistration>, RelayError> {
        let url = {
            let mut url = self.url.clone();
            url.set_path("/relay/v1/data/validator_registration");
            url.set_query(Some(&format!("pubkey={:?}", pubkey)));
            url
        };

        let registration = reqwest::get(url)
            .await?
            .json::<RelayResponse<ValidatorRegistration>>()
            .await?;

        match registration {
            RelayResponse::Ok(registration) => Ok(Some(registration)),
            RelayResponse::Error(error) => {
                if error.code == Some(400) {
                    return Ok(None);
                }
                Err(RelayError::RelayError(error))
            }
        }
    }

    /// Calls /relay/v1/builder/validators to get "validator registrations for validators scheduled to propose in the current and next epoch."
    /// The result will contain the validators for each slot.
    pub async fn get_current_epoch_validators(&self) -> Result<Vec<ValidatorSlotData>, RelayError> {
        let url = {
            let mut url = self.url.clone();
            url.set_path("/relay/v1/builder/validators");
            url
        };

        let req = self.client.get(url);
        let mut headers = HeaderMap::new();
        self.add_auth_headers(&mut headers)
            .map_err(|_| RelayError::InvalidHeader)?;

        let validators = req
            .headers(headers)
            .send()
            .await?
            .json::<RelayResponse<Vec<ValidatorSlotData>>>()
            .await?;

        match validators {
            RelayResponse::Ok(validators) => Ok(validators),
            RelayResponse::Error(error) => Err(RelayError::RelayError(error)),
        }
    }

    /// Mainly takes care of ssz/json raw/gzip
    async fn call_relay_submit_block(
        &self,
        submission_with_metadata: &SubmitBlockRequestWithMetadata,
        ssz: bool,
        gzip: bool,
        fake_relay: bool,
        cancellations: bool,
    ) -> Result<Response, SubmitBlockErr> {
        let url = {
            let mut url = self.url.clone();
            url.set_path("/relay/v1/builder/blocks");
            url.query_pairs_mut()
                .append_pair("cancellations", if cancellations { "1" } else { "0" });
            url
        };

        let mut builder = self.client.post(url.clone());
        let mut headers = HeaderMap::new();
        // SSZ vs JSON
        let (mut body_data, content_type) = if ssz {
            (
                match &submission_with_metadata.submission {
                    SubmitBlockRequest::Capella(data) => data.0.as_ssz_bytes(),
                    SubmitBlockRequest::Deneb(data) => data.0.as_ssz_bytes(),
                    SubmitBlockRequest::Electra(data) => data.0.as_ssz_bytes(),
                },
                SSZ_CONTENT_TYPE,
            )
        } else {
            let json_result = if fake_relay {
                // For the fake relay we remove the blobs
                serde_json::to_vec(&SubmitBlockRequestNoBlobs(
                    &submission_with_metadata.submission,
                ))
            } else {
                serde_json::to_vec(&submission_with_metadata.submission)
            };

            (
                json_result.map_err(|e| SubmitBlockErr::RPCSerializationError(e.to_string()))?,
                JSON_CONTENT_TYPE,
            )
        };
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
        self.add_auth_headers(&mut headers)
            .map_err(|_| SubmitBlockErr::InvalidHeader)?;

        // GZIP
        if gzip {
            headers.insert(
                CONTENT_ENCODING,
                HeaderValue::from_static(GZIP_CONTENT_ENCODING),
            );
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder
                .write_all(&body_data)
                .map_err(|e| SubmitBlockErr::RPCSerializationError(e.to_string()))?;
            body_data = encoder
                .finish()
                .map_err(|e| SubmitBlockErr::RPCSerializationError(e.to_string()))?;
        }

        builder = builder.headers(headers).body(Body::from(body_data));
        if fake_relay {
            builder = builder.header(
                TOTAL_PAYMENT_HEADER,
                submission_with_metadata
                    .metadata
                    .value
                    .coinbase_reward
                    .to_string(),
            );
            if let Some(top_competitor_bid) =
                submission_with_metadata.metadata.value.top_competitor_bid
            {
                builder = builder.header(TOP_BID_HEADER, top_competitor_bid.to_string());
            }
        }

        Ok(builder
            .send()
            .await
            .map_err(|e| RelayError::RequestError(e.into()))?)
    }

    /// Submits the block (call_relay_submit_block) and processes some special errors.
    pub async fn submit_block(
        &self,
        data: &SubmitBlockRequestWithMetadata,
        ssz: bool,
        gzip: bool,
        fake_relay: bool,
        cancellations: bool,
    ) -> Result<(), SubmitBlockErr> {
        let resp = self
            .call_relay_submit_block(data, ssz, gzip, fake_relay, cancellations)
            .await?;
        let status = resp.status();

        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(RelayError::TooManyRequests.into());
        }
        if status == StatusCode::GATEWAY_TIMEOUT {
            return Err(RelayError::ConnectionError.into());
        }

        let data = resp
            .bytes()
            .await
            .map_err(|e| RelayError::RequestError(e.into()))?;

        if status == StatusCode::OK && data.as_ref() == b"" {
            return Ok(());
        }

        match serde_json::from_slice::<RelayResponse<()>>(&data) {
            Ok(RelayResponse::Ok(_)) => Ok(()),
            Ok(RelayResponse::Error(error)) => {
                let msg = error.message.as_str();
                match msg {
                    "payload attributes not (yet) known" => {
                        Err(SubmitBlockErr::PayloadAttributesNotKnown)
                    }
                    "submission for past slot" | "submitted block is for past slot" => {
                        Err(SubmitBlockErr::PastSlot)
                    }
                    "accepted bid below floor, skipped validation" => {
                        Err(SubmitBlockErr::BidBelowFloor)
                    }
                    "payload for this slot was already delivered" => {
                        Err(SubmitBlockErr::PayloadDelivered)
                    }
                    "block already received" => Err(SubmitBlockErr::BlockKnown),
                    _ if msg.contains("read tcp") => Err(RelayError::ConnectionError.into()),
                    _ if msg.contains(SIM_FAILED_SUBSTRING) => {
                        if SIM_FAILED_NON_CRITICAL_ERRORS
                            .iter()
                            .any(|pat| msg.contains(*pat))
                        {
                            Err(RelayError::InternalError.into())
                        } else {
                            Err(SubmitBlockErr::SimError(msg.to_string()))
                        }
                    }
                    _ if msg.contains("request timeout hit") => {
                        Err(RelayError::ConnectionError.into())
                    }
                    _ => Err(RelayError::RelayError(error).into()),
                }
            }
            Err(_) => {
                // bloxroute returns empty response in this format which we handle here because its not valid
                // jsonrpc response
                if data.as_ref() == b"{}\n" {
                    return Ok(());
                }
                let data_string = String::from_utf8_lossy(&data).to_string();
                Err(RelayError::UnknownRelayError(status, data_string).into())
            }
        }
    }

    fn add_auth_headers(&self, headers: &mut HeaderMap) -> eyre::Result<()> {
        if let Some(authorization_header) = &self.authorization_header {
            let mut value = HeaderValue::from_str(authorization_header)?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        if let Some(builder_id_header) = &self.builder_id_header {
            let mut value = HeaderValue::from_str(builder_id_header)?;
            value.set_sensitive(true);
            headers.insert(BUILDER_ID_HEADER, value);
        }
        if let Some(api_token_header) = &self.api_token_header {
            let mut value = HeaderValue::from_str(api_token_header)?;
            value.set_sensitive(true);
            headers.insert(API_TOKEN_HEADER, value);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use submission::{BidMetadata, BidValueMetadata};

    use super::{rpc::TestDataGenerator, *};
    use crate::mev_boost::fake_mev_boost_relay::FakeMevBoostRelay;

    use std::str::FromStr;

    fn create_relay_provider() -> RelayClient {
        RelayClient::from_known_relay(KnownRelay::Flashbots)
    }

    #[tokio::test]
    async fn test_proposer_payload_delivered() {
        let relay = create_relay_provider();

        let result_slot = relay
            .proposer_payload_delivered_slot(7251671)
            .await
            .expect("Failed to get proposer payload delivered, slot");
        let result_block_number = relay
            .proposer_payload_delivered_block_number(18064240)
            .await
            .expect("Failed to get proposer payload delivered, block number");
        let result_block_hash = relay
            .proposer_payload_delivered_block_hash(
                BlockHash::from_str(
                    "0xf2ae3ad64c285ab1de2195f23c19b2b2dcf4949b6f71a4a3406bac9734e1ff27",
                )
                .unwrap(),
            )
            .await
            .expect("Failed to get proposer payload delivered, block hash");

        assert_eq!(result_slot, result_block_number);
        assert_eq!(result_block_number, result_block_hash);

        let result = result_slot.expect("Failed to get proposer payload delivered");

        let expected_result = ProposerPayloadDelivered {
            slot: 7251671,
            parent_hash: BlockHash::from_str("0xe57c063ad96fb5b6fe7696dc8509f3a986ace89d06a19951f3e4404f877bb0ca").unwrap(),
            block_hash: BlockHash::from_str("0xf2ae3ad64c285ab1de2195f23c19b2b2dcf4949b6f71a4a3406bac9734e1ff27").unwrap(),
            builder_pubkey: H384::from_str("0x945fc51bf63613257792926c9155d7ae32db73155dc13bdfe61cd476f1fd2297b66601e8721b723cef11e4e6682e9d87").unwrap(),
            proposer_pubkey: H384::from_str("0xb097a69fa420d01c293fed6b2596778d0722a2b076e401c2789cabce773a17c865285ff71b5dd545c7e77bee6ef8a41b").unwrap(),
            proposer_fee_recipient: Address::from_str("0xeBec795c9c8bBD61FFc14A6662944748F299cAcf").unwrap(),
            gas_limit: 30000000,
            gas_used: 20152932,
            value: U256::from_str("488045688257417849").unwrap(),
            num_tx: 168,
            block_number: 18064240,
        };

        assert_eq!(result, expected_result)
    }

    #[tokio::test]
    async fn test_builder_block_received() {
        let relay = create_relay_provider();
        let result = relay
            .builder_block_received_block_hash(
                BlockHash::from_str(
                    "0xae52fd69bb83fbe20802b8c130bed111d2f0b9620ab6d8ee369eee11e15b845e",
                )
                .unwrap(),
            )
            .await
            .expect("Failed to get builder block received")
            .expect("Builder block received not found");

        let expected_result = BuilderBlockReceived {
            slot: 7251671,
            parent_hash: BlockHash::from_str("0xe57c063ad96fb5b6fe7696dc8509f3a986ace89d06a19951f3e4404f877bb0ca").unwrap(),
            block_hash: BlockHash::from_str("0xae52fd69bb83fbe20802b8c130bed111d2f0b9620ab6d8ee369eee11e15b845e").unwrap(),
            builder_pubkey: H384::from_str("0x945fc51bf63613257792926c9155d7ae32db73155dc13bdfe61cd476f1fd2297b66601e8721b723cef11e4e6682e9d87").unwrap(),
            proposer_pubkey: H384::from_str("0xb097a69fa420d01c293fed6b2596778d0722a2b076e401c2789cabce773a17c865285ff71b5dd545c7e77bee6ef8a41b").unwrap(),
            proposer_fee_recipient: Address::from_str("0xeBec795c9c8bBD61FFc14A6662944748F299cAcf").unwrap(),
            gas_limit: 30000000,
            gas_used: 20585314,
            value: U256::from_str("513749883680431063").unwrap(),
            num_tx: 174,
            block_number: 18064240,
            timestamp: 1693844075,
            timestamp_ms: 1693844075794,
            optimistic_submission: false,
        };

        assert_eq!(result, expected_result)
    }

    #[tokio::test]
    async fn test_validator_registration() {
        let relay = create_relay_provider();
        let result = relay
            .validator_registration(H384::from_str("0x904e01b37a8db98695483da0025f4cc1c0ecc2b9a37ab53604c21d6788ad2a8996748ea0ce838d2926767eee0228ec69").unwrap())
            .await
            .expect("Failed to get validator registration")
        .expect("Validator registration not found");

        let expected_result = ValidatorRegistration {
            message: ValidatorRegistrationMessage {
                fee_recipient: Address::from_str("0x388c818ca8b9251b393131c08a736a67ccb19297").unwrap(),
                gas_limit: 30000000,
                timestamp: 1707146537,
                pubkey: H384::from_str("0x904e01b37a8db98695483da0025f4cc1c0ecc2b9a37ab53604c21d6788ad2a8996748ea0ce838d2926767eee0228ec69").unwrap(),
            },
            signature: Bytes::from_str("0xa1d5118053d39665319909d099db64f174e62ffee22c4c65f7c86f549bde1225148af3e4623dc17f2bf8a292249693bb0556ef3bd13b7ea79fe58298e3a97f93c225777376ee07e49e8db9265a32716306288f5117f24bbac445babc78a81888").unwrap(),
        };

        assert_eq!(result, expected_result)
    }

    #[tokio::test]
    async fn test_bids_without_optimistic_submission() {
        let data = "[{\"slot\":\"7397378\",\"parent_hash\":\"0x71927975871e10dfd63e773d46836a7fac9357dba02e38a5fb31532392398646\",\"block_hash\":\"0xd32c5a6b5049934cd403390465d31ed8653e3b91e7ff1ace0eb0c59cc4a15274\",\"builder_pubkey\":\"0x945fc51bf63613257792926c9155d7ae32db73155dc13bdfe61cd476f1fd2297b66601e8721b723cef11e4e6682e9d87\",\"proposer_pubkey\":\"0x98503b20c69e073669e3291a947a4c5afb91d38bd8f8c562d60ce67ad1eb68c97c04eea5cfcb39a1d398bfb86f4dc961\",\"proposer_fee_recipient\":\"0x13ad787D1F74302C6301CCf725B8688599B06da0\",\"gas_limit\":\"30000000\",\"gas_used\":\"21739227\",\"value\":\"140212076843484261\",\"num_tx\":\"194\",\"block_number\":\"18208474\",\"timestamp_ms\":\"1695592558964\",\"timestamp\":\"1695592558\"}]\n";

        let result = serde_json::from_str::<Vec<BuilderBlockReceived>>(data).unwrap();
        assert_eq!(result.len(), 1);
        assert!(!result[0].optimistic_submission);
    }

    #[tokio::test]
    async fn test_validator_slot_data() {
        let relay = create_relay_provider();

        let result = relay
            .get_current_epoch_validators()
            .await
            .expect("Failed to get validators");

        println!("len: {}", result.len());
        assert!(!result.is_empty());
        println!("result[0]: {:#?}", result[0]);
    }

    #[ignore]
    #[tokio::test]
    async fn test_send_payload_to_mevboost() {
        let srv = match FakeMevBoostRelay::new().spawn() {
            Some(srv) => srv,
            None => {
                println!("mev-boost binary not found, skipping test");
                return;
            }
        };

        let mut generator = TestDataGenerator::default();

        let relay_url = Url::from_str(&srv.endpoint()).unwrap();
        let relay = RelayClient::from_url(relay_url, None, None, None);
        let submission = SubmitBlockRequest::Deneb(generator.create_deneb_submit_block_request());
        let sub_relay = SubmitBlockRequestWithMetadata {
            submission,
            metadata: BidMetadata {
                value: BidValueMetadata {
                    coinbase_reward: Default::default(),
                    top_competitor_bid: None,
                },
            },
        };
        relay
            .submit_block(&sub_relay, true, true, false, false)
            .await
            .expect("OPS!");
    }
}
