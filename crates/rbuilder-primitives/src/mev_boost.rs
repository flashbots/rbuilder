use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_beacon::{
    relay::{
        BidTrace, SignedBidSubmissionV2, SignedBidSubmissionV3, SignedBidSubmissionV4,
        SignedBidSubmissionV5,
    },
    requests::ExecutionRequestsV4,
    BlsPublicKey, BlsSignature,
};
use alloy_rpc_types_engine::{
    BlobsBundleV1, BlobsBundleV2, ExecutionPayloadV2, ExecutionPayloadV3,
};
use derive_more::Deref;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use std::time::Duration;

use crate::OrderId;

/// Usually human readable id for relays. Not used on anything on any protocol just to identify the relays.
pub type MevBoostRelayID = String;

/// Timeout for requesting current epoch data from the MEV-Boost relay.
pub const MEV_BOOST_SLOT_INFO_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Modes for a relay since we may use them for different purposes.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq, Default)]
pub enum RelayMode {
    /// Submits bids, gets slot info. No extra headers on bidding.
    #[serde(rename = "full")]
    #[default]
    Full,
    /// Only gets slot info.
    #[serde(rename = "slot_info")]
    GetSlotInfoOnly,
    /// Submits bids with extra headers. Is not used to get slot info.
    #[serde(rename = "test")]
    Test,
}

impl RelayMode {
    pub fn submits_bids(&self) -> bool {
        match self {
            RelayMode::Full => true,
            RelayMode::GetSlotInfoOnly => false,
            RelayMode::Test => true,
        }
    }
    pub fn gets_slot_info(&self) -> bool {
        match self {
            RelayMode::Full => true,
            RelayMode::GetSlotInfoOnly => true,
            RelayMode::Test => false,
        }
    }
}

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

    pub fn is_bloxroute(&self) -> bool {
        matches!(
            self,
            Self::BloxrouteMaxProfit | Self::BloxrouteEthical | Self::BloxrouteRegulated
        )
    }
}

impl std::str::FromStr for KnownRelay {
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

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Hash)]
pub struct ValidatorRegistrationMessage {
    pub fee_recipient: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub gas_limit: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub timestamp: u64,
    pub pubkey: BlsPublicKey,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Hash)]
pub struct ValidatorRegistration {
    pub message: ValidatorRegistrationMessage,
    pub signature: Bytes,
}

/// Info about a registered validator selected as proposer for a slot.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Hash)]
pub struct ValidatorSlotData {
    /// The slot number for the validator entry.
    #[serde_as(as = "DisplayFromStr")]
    pub slot: u64,
    /// The index of the validator.
    #[serde_as(as = "DisplayFromStr")]
    pub validator_index: u64,
    /// Details of the validator registration.
    pub entry: ValidatorRegistration,
    /// (Bloxroute) Collection of regional endpoints validator is connected to.
    #[serde(default)]
    pub regional_endpoints: Vec<BloxrouteRegionalEndpoint>,
}

/// Bloxroute validator RProxy details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct BloxrouteRegionalEndpoint {
    /// RProxy name
    pub name: String,
    /// RProxy region. Format: `city,region`.
    pub region: String,
    /// RProxy HTTP endpoint.
    pub http_endpoint: String,
    /// RProxy gRPC endpoint.
    pub grpc_endpoint: String,
    /// RProxy WS endpoint.
    pub websocket_endpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ssz_derive::Encode)]
#[ssz(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum SubmitBlockRequest {
    Fulu(FuluSubmitBlockRequest),
    Capella(CapellaSubmitBlockRequest),
    Deneb(DenebSubmitBlockRequest),
    Electra(ElectraSubmitBlockRequest),
}

impl SubmitBlockRequest {
    #[inline]
    pub fn capella(request: CapellaSubmitBlockRequest) -> Self {
        Self::Capella(request)
    }

    #[inline]
    pub fn deneb(request: DenebSubmitBlockRequest) -> Self {
        Self::Deneb(request)
    }

    #[inline]
    pub fn electra(request: ElectraSubmitBlockRequest) -> Self {
        Self::Electra(request)
    }

    #[inline]
    pub fn fulu(request: FuluSubmitBlockRequest) -> Self {
        Self::Fulu(request)
    }

    pub fn bid_trace(&self) -> &BidTrace {
        match self {
            SubmitBlockRequest::Fulu(req) => &req.message,
            SubmitBlockRequest::Capella(req) => &req.message,
            SubmitBlockRequest::Deneb(req) => &req.message,
            SubmitBlockRequest::Electra(req) => &req.message,
        }
    }

    pub fn has_adjustment_data(&self) -> bool {
        let maybe_adjustment_data = match self {
            SubmitBlockRequest::Capella(req) => &req.adjustment_data,
            SubmitBlockRequest::Deneb(req) => &req.adjustment_data,
            SubmitBlockRequest::Electra(req) => &req.adjustment_data,
            SubmitBlockRequest::Fulu(req) => &req.adjustment_data,
        };
        maybe_adjustment_data.is_some()
    }
}

impl ssz::Decode for SubmitBlockRequest {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        if let Ok(result) = FuluSubmitBlockRequest::from_ssz_bytes(bytes) {
            return Ok(Self::fulu(result));
        }
        if let Ok(result) = ElectraSubmitBlockRequest::from_ssz_bytes(bytes) {
            return Ok(Self::electra(result));
        }
        if let Ok(result) = DenebSubmitBlockRequest::from_ssz_bytes(bytes) {
            return Ok(Self::deneb(result));
        }

        let result = CapellaSubmitBlockRequest::from_ssz_bytes(bytes)?;
        Ok(Self::capella(result))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Deref)]
pub struct FuluSubmitBlockRequest {
    #[deref]
    #[serde(flatten)]
    pub submission: SignedBidSubmissionV5,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjustment_data: Option<BidAdjustmentData>,
}

impl FuluSubmitBlockRequest {
    pub fn new(
        submission: SignedBidSubmissionV5,
        adjustment_data: Option<BidAdjustmentData>,
    ) -> Self {
        Self {
            submission,
            adjustment_data,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Deref)]
pub struct ElectraSubmitBlockRequest {
    /// Inner bid submission.
    #[deref]
    #[serde(flatten)]
    pub submission: SignedBidSubmissionV4,
    /// Bid adjustment data if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjustment_data: Option<BidAdjustmentData>,
}

impl ElectraSubmitBlockRequest {
    /// Create new Electra submit block request.
    pub fn new(
        submission: SignedBidSubmissionV4,
        adjustment_data: Option<BidAdjustmentData>,
    ) -> Self {
        Self {
            submission,
            adjustment_data,
        }
    }
}

impl ssz::Encode for FuluSubmitBlockRequest {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        let mut offset = <BidTrace as ssz::Encode>::ssz_fixed_len()
            + <ExecutionPayloadV3 as ssz::Encode>::ssz_fixed_len()
            + <BlobsBundleV2 as ssz::Encode>::ssz_fixed_len()
            + <ExecutionRequestsV4 as ssz::Encode>::ssz_fixed_len()
            + <BlsSignature as ssz::Encode>::ssz_fixed_len();
        if self.adjustment_data.is_some() {
            offset += <BidAdjustmentData as ssz::Encode>::ssz_fixed_len();
        }

        let mut encoder = ssz::SszEncoder::container(buf, offset);

        encoder.append(&self.message);
        encoder.append(&self.execution_payload);
        encoder.append(&self.blobs_bundle);
        encoder.append(&self.execution_requests);
        encoder.append(&self.signature);
        if let Some(adjustment) = &self.adjustment_data {
            encoder.append(&adjustment);
        }

        encoder.finalize();
    }

    fn ssz_bytes_len(&self) -> usize {
        let mut len = <BidTrace as ssz::Encode>::ssz_bytes_len(&self.message)
            + <ExecutionPayloadV3 as ssz::Encode>::ssz_bytes_len(&self.execution_payload)
            + <BlobsBundleV2 as ssz::Encode>::ssz_bytes_len(&self.blobs_bundle)
            + <ExecutionRequestsV4 as ssz::Encode>::ssz_bytes_len(&self.execution_requests)
            + <BlsSignature as ssz::Encode>::ssz_bytes_len(&self.signature);
        if let Some(adjustment) = &self.adjustment_data {
            len += <BidAdjustmentData as ssz::Encode>::ssz_bytes_len(adjustment);
        }
        len
    }
}

impl ssz::Decode for FuluSubmitBlockRequest {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        #[derive(ssz_derive::Decode)]
        struct FuluSubmitBlockRequestSszHelper {
            message: BidTrace,
            execution_payload: ExecutionPayloadV3,
            blobs_bundle: BlobsBundleV2,
            execution_requests: ExecutionRequestsV4,
            signature: BlsSignature,
            adjustment_data: BidAdjustmentData,
        }

        if let Ok(request) = FuluSubmitBlockRequestSszHelper::from_ssz_bytes(bytes) {
            let FuluSubmitBlockRequestSszHelper {
                message,
                execution_payload,
                blobs_bundle,
                execution_requests,
                signature,
                adjustment_data,
            } = request;
            let submission = SignedBidSubmissionV5 {
                message,
                execution_payload,
                blobs_bundle,
                execution_requests,
                signature,
            };
            Ok(Self::new(submission, Some(adjustment_data)))
        } else {
            let submission = SignedBidSubmissionV5::from_ssz_bytes(bytes)?;
            Ok(Self::new(submission, None))
        }
    }
}

impl ssz::Encode for ElectraSubmitBlockRequest {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        let mut offset = <BidTrace as ssz::Encode>::ssz_fixed_len()
            + <ExecutionPayloadV3 as ssz::Encode>::ssz_fixed_len()
            + <BlobsBundleV1 as ssz::Encode>::ssz_fixed_len()
            + <ExecutionRequestsV4 as ssz::Encode>::ssz_fixed_len()
            + <BlsSignature as ssz::Encode>::ssz_fixed_len();
        if self.adjustment_data.is_some() {
            offset += <BidAdjustmentData as ssz::Encode>::ssz_fixed_len();
        }

        let mut encoder = ssz::SszEncoder::container(buf, offset);

        encoder.append(&self.message);
        encoder.append(&self.execution_payload);
        encoder.append(&self.blobs_bundle);
        encoder.append(&self.execution_requests);
        encoder.append(&self.signature);
        if let Some(adjustment) = &self.adjustment_data {
            encoder.append(&adjustment);
        }

        encoder.finalize();
    }

    fn ssz_bytes_len(&self) -> usize {
        let mut len = <BidTrace as ssz::Encode>::ssz_bytes_len(&self.message)
            + <ExecutionPayloadV3 as ssz::Encode>::ssz_bytes_len(&self.execution_payload)
            + <BlobsBundleV1 as ssz::Encode>::ssz_bytes_len(&self.blobs_bundle)
            + <ExecutionRequestsV4 as ssz::Encode>::ssz_bytes_len(&self.execution_requests)
            + <BlsSignature as ssz::Encode>::ssz_bytes_len(&self.signature);
        if let Some(adjustment) = &self.adjustment_data {
            len += <BidAdjustmentData as ssz::Encode>::ssz_bytes_len(adjustment);
        }
        len
    }
}

impl ssz::Decode for ElectraSubmitBlockRequest {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        #[derive(ssz_derive::Decode)]
        struct ElectraSubmitBlockRequestSszHelper {
            message: BidTrace,
            execution_payload: ExecutionPayloadV3,
            blobs_bundle: BlobsBundleV1,
            execution_requests: ExecutionRequestsV4,
            signature: BlsSignature,
            adjustment_data: BidAdjustmentData,
        }

        if let Ok(request) = ElectraSubmitBlockRequestSszHelper::from_ssz_bytes(bytes) {
            let ElectraSubmitBlockRequestSszHelper {
                message,
                execution_payload,
                blobs_bundle,
                execution_requests,
                signature,
                adjustment_data,
            } = request;
            let submission = SignedBidSubmissionV4 {
                message,
                execution_payload,
                blobs_bundle,
                execution_requests,
                signature,
            };
            Ok(Self::new(submission, Some(adjustment_data)))
        } else {
            let submission = SignedBidSubmissionV4::from_ssz_bytes(bytes)?;
            Ok(Self::new(submission, None))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Deref)]
pub struct DenebSubmitBlockRequest {
    /// Inner bid submission.
    #[deref]
    #[serde(flatten)]
    pub submission: SignedBidSubmissionV3,
    /// Bid adjustment data if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjustment_data: Option<BidAdjustmentData>,
}

impl DenebSubmitBlockRequest {
    /// Create new Deneb submit block request.
    pub fn new(
        submission: SignedBidSubmissionV3,
        adjustment_data: Option<BidAdjustmentData>,
    ) -> Self {
        Self {
            submission,
            adjustment_data,
        }
    }
}

impl ssz::Encode for DenebSubmitBlockRequest {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        let mut offset = <BidTrace as ssz::Encode>::ssz_fixed_len()
            + <ExecutionPayloadV3 as ssz::Encode>::ssz_fixed_len()
            + <BlobsBundleV1 as ssz::Encode>::ssz_fixed_len()
            + <BlsSignature as ssz::Encode>::ssz_fixed_len();
        if self.adjustment_data.is_some() {
            offset += <BidAdjustmentData as ssz::Encode>::ssz_fixed_len();
        }

        let mut encoder = ssz::SszEncoder::container(buf, offset);

        encoder.append(&self.message);
        encoder.append(&self.execution_payload);
        encoder.append(&self.blobs_bundle);
        encoder.append(&self.signature);
        if let Some(adjustment) = &self.adjustment_data {
            encoder.append(&adjustment);
        }

        encoder.finalize();
    }

    fn ssz_bytes_len(&self) -> usize {
        let mut len = <BidTrace as ssz::Encode>::ssz_bytes_len(&self.message)
            + <ExecutionPayloadV3 as ssz::Encode>::ssz_bytes_len(&self.execution_payload)
            + <BlobsBundleV1 as ssz::Encode>::ssz_bytes_len(&self.blobs_bundle)
            + <BlsSignature as ssz::Encode>::ssz_bytes_len(&self.signature);
        if let Some(adjustment) = &self.adjustment_data {
            len += <BidAdjustmentData as ssz::Encode>::ssz_bytes_len(adjustment);
        }
        len
    }
}

impl ssz::Decode for DenebSubmitBlockRequest {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        #[derive(ssz_derive::Decode)]
        struct DenebSubmitBlockRequestSszHelper {
            message: BidTrace,
            execution_payload: ExecutionPayloadV3,
            blobs_bundle: BlobsBundleV1,
            signature: BlsSignature,
            adjustment_data: BidAdjustmentData,
        }

        if let Ok(request) = DenebSubmitBlockRequestSszHelper::from_ssz_bytes(bytes) {
            let DenebSubmitBlockRequestSszHelper {
                message,
                execution_payload,
                blobs_bundle,
                signature,
                adjustment_data,
            } = request;
            let submission = SignedBidSubmissionV3 {
                message,
                execution_payload,
                blobs_bundle,
                signature,
            };
            Ok(Self::new(submission, Some(adjustment_data)))
        } else {
            let submission = SignedBidSubmissionV3::from_ssz_bytes(bytes)?;
            Ok(Self::new(submission, None))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Deref)]
pub struct CapellaSubmitBlockRequest {
    /// Inner bid submission.
    #[deref]
    #[serde(flatten)]
    pub submission: SignedBidSubmissionV2,
    /// Bid adjustment data if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjustment_data: Option<BidAdjustmentData>,
}

impl CapellaSubmitBlockRequest {
    /// Create new Capella submit block request.
    pub fn new(
        submission: SignedBidSubmissionV2,
        adjustment_data: Option<BidAdjustmentData>,
    ) -> Self {
        Self {
            submission,
            adjustment_data,
        }
    }
}

impl ssz::Encode for CapellaSubmitBlockRequest {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        let mut offset = <BidTrace as ssz::Encode>::ssz_fixed_len()
            + <ExecutionPayloadV2 as ssz::Encode>::ssz_fixed_len()
            + <BlsSignature as ssz::Encode>::ssz_fixed_len();
        if self.adjustment_data.is_some() {
            offset += <BidAdjustmentData as ssz::Encode>::ssz_fixed_len();
        }

        let mut encoder = ssz::SszEncoder::container(buf, offset);
        encoder.append(&self.message);
        encoder.append(&self.execution_payload);
        encoder.append(&self.signature);
        if let Some(adjustment) = &self.adjustment_data {
            encoder.append(&adjustment);
        }

        encoder.finalize();
    }

    fn ssz_bytes_len(&self) -> usize {
        let mut len = <BidTrace as ssz::Encode>::ssz_bytes_len(&self.message)
            + <ExecutionPayloadV2 as ssz::Encode>::ssz_bytes_len(&self.execution_payload)
            + <BlsSignature as ssz::Encode>::ssz_bytes_len(&self.signature);
        if let Some(adjustment) = &self.adjustment_data {
            len += <BidAdjustmentData as ssz::Encode>::ssz_bytes_len(adjustment);
        }
        len
    }
}

impl ssz::Decode for CapellaSubmitBlockRequest {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        #[derive(ssz_derive::Decode)]
        struct CapellaSubmitBlockRequestSszHelper {
            message: BidTrace,
            execution_payload: ExecutionPayloadV2,
            signature: BlsSignature,
            adjustment_data: BidAdjustmentData,
        }

        if let Ok(request) = CapellaSubmitBlockRequestSszHelper::from_ssz_bytes(bytes) {
            let CapellaSubmitBlockRequestSszHelper {
                message,
                execution_payload,
                signature,
                adjustment_data,
            } = request;
            let submission = SignedBidSubmissionV2 {
                message,
                execution_payload,
                signature,
            };
            Ok(Self::new(submission, Some(adjustment_data)))
        } else {
            let submission = SignedBidSubmissionV2::from_ssz_bytes(bytes)?;
            Ok(Self::new(submission, None))
        }
    }
}

#[derive(Clone, Debug)]
pub struct BidMetadata {
    pub value: BidValueMetadata,
    pub order_ids: Vec<OrderId>,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct BidValueMetadata {
    pub coinbase_reward: U256,
    pub top_competitor_bid: Option<U256>,
}

#[derive(Clone, Debug)]
pub struct SubmitBlockRequestWithMetadata {
    pub submission: SubmitBlockRequest,
    pub metadata: BidMetadata,
}

/// Signed bid submission that is serialized without blobs bundle.
#[derive(Debug)]
pub struct SubmitBlockRequestNoBlobs<'a>(pub &'a SubmitBlockRequest);

impl serde::Serialize for SubmitBlockRequestNoBlobs<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            SubmitBlockRequest::Capella(v2) => v2.serialize(serializer),
            SubmitBlockRequest::Deneb(v3) => {
                #[derive(serde::Serialize)]
                struct SignedBidSubmissionV3Ref<'a> {
                    message: &'a BidTrace,
                    #[serde(with = "alloy_rpc_types_beacon::payload::beacon_payload_v3")]
                    execution_payload: &'a ExecutionPayloadV3,
                    blobs_bundle: &'a BlobsBundleV1,
                    signature: &'a BlsSignature,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    adjustment_data: &'a Option<BidAdjustmentData>,
                }

                SignedBidSubmissionV3Ref {
                    message: &v3.message,
                    execution_payload: &v3.execution_payload,
                    blobs_bundle: &BlobsBundleV1::new([]), // override blobs bundle with empty one
                    signature: &v3.signature,
                    adjustment_data: &v3.adjustment_data,
                }
                .serialize(serializer)
            }
            SubmitBlockRequest::Electra(v4) => {
                #[derive(serde::Serialize)]
                struct SignedBidSubmissionV4Ref<'a> {
                    message: &'a BidTrace,
                    #[serde(with = "alloy_rpc_types_beacon::payload::beacon_payload_v3")]
                    execution_payload: &'a ExecutionPayloadV3,
                    blobs_bundle: &'a BlobsBundleV1,
                    execution_requests: &'a ExecutionRequestsV4,
                    signature: &'a BlsSignature,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    adjustment_data: &'a Option<BidAdjustmentData>,
                }

                SignedBidSubmissionV4Ref {
                    message: &v4.message,
                    execution_payload: &v4.execution_payload,
                    blobs_bundle: &BlobsBundleV1::new([]), // override blobs bundle with empty one
                    signature: &v4.signature,
                    execution_requests: &v4.execution_requests,
                    adjustment_data: &v4.adjustment_data,
                }
                .serialize(serializer)
            }
            SubmitBlockRequest::Fulu(v5) => {
                #[derive(serde::Serialize)]
                struct SignedBidSubmissionV5Ref<'a> {
                    message: &'a BidTrace,
                    #[serde(with = "alloy_rpc_types_beacon::payload::beacon_payload_v3")]
                    execution_payload: &'a ExecutionPayloadV3,
                    blobs_bundle: &'a BlobsBundleV2,
                    execution_requests: &'a ExecutionRequestsV4,
                    signature: &'a BlsSignature,
                }

                SignedBidSubmissionV5Ref {
                    message: &v5.message,
                    execution_payload: &v5.execution_payload,
                    blobs_bundle: &BlobsBundleV2::new([]), // override blobs bundle with empty one
                    signature: &v5.signature,
                    execution_requests: &v5.execution_requests,
                }
                .serialize(serializer)
            }
        }
    }
}

/// The type representing UltraSound bid adjustments.
#[derive(
    PartialEq,
    Eq,
    Clone,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    ssz_derive::Encode,
    ssz_derive::Decode,
)]
pub struct BidAdjustmentData {
    /// State root of the payload.
    pub state_root: B256,
    /// Transactions root of the payload.
    pub transactions_root: B256,
    /// Receipts root of the payload.
    pub receipts_root: B256,
    /// The usual builder address that pays the proposer in the last transaction of the block.
    /// When we adjust a bid, this transaction is overwritten by a transaction from the collateral
    /// account `fee_payer_address`. If we don't adjust the bid, `builder_address` pays the
    /// proposer as per usual.
    pub builder_address: Address,
    /// The state proof for the builder account.
    pub builder_proof: Vec<Bytes>,
    /// The proposer's fee recipient.
    pub fee_recipient_address: Address,
    /// The state proof for the fee recipient account.
    pub fee_recipient_proof: Vec<Bytes>,
    /// The fee payer address that is custodied by the relay.
    pub fee_payer_address: Address,
    /// The state proof for the fee payer account.
    pub fee_payer_proof: Vec<Bytes>,
    /// The merkle proof for the last transaction in the block, which will be overwritten with a
    /// payment from `fee_payer` to `fee_recipient` if we adjust the bid.
    pub placeholder_transaction_proof: Vec<Bytes>,
    /// The merkle proof for the receipt of the placeholder transaction. It's required for
    /// adjusting payments to contract addresses.
    pub placeholder_receipt_proof: Vec<Bytes>,
}
