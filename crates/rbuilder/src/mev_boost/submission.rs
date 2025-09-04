use alloy_primitives::U256;
use alloy_rpc_types_beacon::{
    relay::{
        BidTrace, SignedBidSubmissionV2, SignedBidSubmissionV3, SignedBidSubmissionV4,
        SignedBidSubmissionV5,
    },
    requests::ExecutionRequestsV4,
    BlsSignature,
};
use alloy_rpc_types_engine::{
    BlobsBundleV1, BlobsBundleV2, ExecutionPayloadV2, ExecutionPayloadV3,
};
use derive_more::Deref;
use serde::{Deserialize, Serialize};
use ssz::DecodeError;

use super::adjustment::BidAdjustmentData;
use crate::primitives::OrderId;

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

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
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

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
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

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
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

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
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

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
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
