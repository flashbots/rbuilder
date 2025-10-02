use crate::mev_boost::BidAdjustmentDataV1;
use alloy_rpc_types_beacon::{
    relay::{
        BidTrace, SignedBidSubmissionV2, SignedBidSubmissionV3, SignedBidSubmissionV4,
        SignedBidSubmissionV5,
    },
    requests::ExecutionRequestsV4,
    BlsSignature,
};
use alloy_rpc_types_engine::{
    BlobsBundleV1, BlobsBundleV2, ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3,
};
use derive_more::Deref;
use serde::{Deserialize, Serialize};

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
            SubmitBlockRequest::Capella(req) => &req.message,
            SubmitBlockRequest::Deneb(req) => &req.message,
            SubmitBlockRequest::Electra(req) => &req.message,
            SubmitBlockRequest::Fulu(req) => &req.message,
        }
    }

    pub fn signature(&self) -> BlsSignature {
        match self {
            SubmitBlockRequest::Capella(req) => req.signature,
            SubmitBlockRequest::Deneb(req) => req.signature,
            SubmitBlockRequest::Electra(req) => req.signature,
            SubmitBlockRequest::Fulu(req) => req.signature,
        }
    }

    pub fn execution_payload_v1(&self) -> &ExecutionPayloadV1 {
        match self {
            SubmitBlockRequest::Capella(req) => &req.execution_payload.payload_inner,
            SubmitBlockRequest::Deneb(req) => &req.execution_payload.payload_inner.payload_inner,
            SubmitBlockRequest::Electra(req) => &req.execution_payload.payload_inner.payload_inner,
            SubmitBlockRequest::Fulu(req) => &req.execution_payload.payload_inner.payload_inner,
        }
    }

    pub fn execution_payload_v2(&self) -> &ExecutionPayloadV2 {
        match self {
            SubmitBlockRequest::Capella(req) => &req.execution_payload,
            SubmitBlockRequest::Deneb(req) => &req.execution_payload.payload_inner,
            SubmitBlockRequest::Electra(req) => &req.execution_payload.payload_inner,
            SubmitBlockRequest::Fulu(req) => &req.execution_payload.payload_inner,
        }
    }

    pub fn execution_payload_v3(&self) -> Option<&ExecutionPayloadV3> {
        match self {
            SubmitBlockRequest::Capella(_) => None,
            SubmitBlockRequest::Deneb(req) => Some(&req.execution_payload),
            SubmitBlockRequest::Electra(req) => Some(&req.execution_payload),
            SubmitBlockRequest::Fulu(req) => Some(&req.execution_payload),
        }
    }

    /// Returns `true` if block has adjustment data.
    pub fn has_adjustment_data(&self) -> bool {
        let maybe_adjustment_data = match self {
            SubmitBlockRequest::Capella(req) => &req.adjustment_data,
            SubmitBlockRequest::Deneb(req) => &req.adjustment_data,
            SubmitBlockRequest::Electra(req) => &req.adjustment_data,
            SubmitBlockRequest::Fulu(req) => &req.adjustment_data,
        };
        maybe_adjustment_data.is_some()
    }

    /// Return mutable reference to bid adjustment data.
    fn adjustment_data_mut(&mut self) -> &mut Option<BidAdjustmentDataV1> {
        match self {
            Self::Capella(CapellaSubmitBlockRequest {
                adjustment_data, ..
            })
            | Self::Deneb(DenebSubmitBlockRequest {
                adjustment_data, ..
            })
            | Self::Electra(ElectraSubmitBlockRequest {
                adjustment_data, ..
            })
            | Self::Fulu(FuluSubmitBlockRequest {
                adjustment_data, ..
            }) => adjustment_data,
        }
    }

    /// Set the bid adjustment data on the request.
    pub fn set_adjustment_data(&mut self, data: BidAdjustmentDataV1) {
        *self.adjustment_data_mut() = Some(data);
    }

    /// Remove adjustment data from the bid.
    pub fn remove_adjustment_data(&mut self) {
        self.adjustment_data_mut().take();
    }

    /// Remove adjustment data from the bid and return it.
    pub fn without_adjustment_data(mut self) -> Self {
        self.remove_adjustment_data();
        self
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
    pub adjustment_data: Option<BidAdjustmentDataV1>,
}

impl FuluSubmitBlockRequest {
    pub fn new(
        submission: SignedBidSubmissionV5,
        adjustment_data: Option<BidAdjustmentDataV1>,
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
    pub adjustment_data: Option<BidAdjustmentDataV1>,
}

impl ElectraSubmitBlockRequest {
    /// Create new Electra submit block request.
    pub fn new(
        submission: SignedBidSubmissionV4,
        adjustment_data: Option<BidAdjustmentDataV1>,
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
            offset += <BidAdjustmentDataV1 as ssz::Encode>::ssz_fixed_len();
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
            len += <BidAdjustmentDataV1 as ssz::Encode>::ssz_bytes_len(adjustment);
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
            adjustment_data: BidAdjustmentDataV1,
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
            offset += <BidAdjustmentDataV1 as ssz::Encode>::ssz_fixed_len();
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
            len += <BidAdjustmentDataV1 as ssz::Encode>::ssz_bytes_len(adjustment);
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
            adjustment_data: BidAdjustmentDataV1,
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
    pub adjustment_data: Option<BidAdjustmentDataV1>,
}

impl DenebSubmitBlockRequest {
    /// Create new Deneb submit block request.
    pub fn new(
        submission: SignedBidSubmissionV3,
        adjustment_data: Option<BidAdjustmentDataV1>,
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
            offset += <BidAdjustmentDataV1 as ssz::Encode>::ssz_fixed_len();
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
            len += <BidAdjustmentDataV1 as ssz::Encode>::ssz_bytes_len(adjustment);
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
            adjustment_data: BidAdjustmentDataV1,
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
    pub adjustment_data: Option<BidAdjustmentDataV1>,
}

impl CapellaSubmitBlockRequest {
    /// Create new Capella submit block request.
    pub fn new(
        submission: SignedBidSubmissionV2,
        adjustment_data: Option<BidAdjustmentDataV1>,
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
            offset += <BidAdjustmentDataV1 as ssz::Encode>::ssz_fixed_len();
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
            len += <BidAdjustmentDataV1 as ssz::Encode>::ssz_bytes_len(adjustment);
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
            adjustment_data: BidAdjustmentDataV1,
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
