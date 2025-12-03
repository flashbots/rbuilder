use crate::mev_boost::BidAdjustmentDataV1;
use alloy_rpc_types_beacon::{
    relay::{
        BidTrace, SignedBidSubmissionV2, SignedBidSubmissionV3, SignedBidSubmissionV4,
        SignedBidSubmissionV5, SubmitBlockRequest as AlloySubmitBlockRequest,
    },
    requests::ExecutionRequestsV4,
    BlsSignature,
};
use alloy_rpc_types_engine::{
    BlobsBundleV1, BlobsBundleV2, ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3,
};
use derive_more::Deref;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, Deref)]
pub struct SubmitBlockRequest {
    /// Inner submit block request.
    #[deref]
    #[serde(flatten)]
    pub request: Arc<AlloySubmitBlockRequest>,
    /// Bid adjustment data if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjustment_data: Option<BidAdjustmentDataV1>,
}

impl ssz::Encode for SubmitBlockRequest {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        // Every request contains bid trace and signature.
        let mut offset = <BidTrace as ssz::Encode>::ssz_fixed_len()
            + <BlsSignature as ssz::Encode>::ssz_fixed_len();
        // Amend offset with fork specific fields.
        offset += match &self.request.as_ref() {
            AlloySubmitBlockRequest::Fulu(_) => {
                <ExecutionPayloadV3 as ssz::Encode>::ssz_fixed_len()
                    + <BlobsBundleV2 as ssz::Encode>::ssz_fixed_len()
                    + <ExecutionRequestsV4 as ssz::Encode>::ssz_fixed_len()
            }
            AlloySubmitBlockRequest::Electra(_) => {
                <ExecutionPayloadV3 as ssz::Encode>::ssz_fixed_len()
                    + <BlobsBundleV1 as ssz::Encode>::ssz_fixed_len()
                    + <ExecutionRequestsV4 as ssz::Encode>::ssz_fixed_len()
            }
            AlloySubmitBlockRequest::Deneb(_) => {
                <ExecutionPayloadV3 as ssz::Encode>::ssz_fixed_len()
                    + <BlobsBundleV1 as ssz::Encode>::ssz_fixed_len()
            }
            AlloySubmitBlockRequest::Capella(_) => {
                <ExecutionPayloadV2 as ssz::Encode>::ssz_fixed_len()
            }
        };
        // Add adjustment data offset if present.
        if self.adjustment_data.is_some() {
            offset += <BidAdjustmentDataV1 as ssz::Encode>::ssz_fixed_len();
        }

        let mut encoder = ssz::SszEncoder::container(buf, offset);
        match self.request.as_ref() {
            AlloySubmitBlockRequest::Fulu(request) => {
                let SignedBidSubmissionV5 {
                    message,
                    execution_payload,
                    blobs_bundle,
                    execution_requests,
                    signature,
                } = request;
                encoder.append(&message);
                encoder.append(&execution_payload);
                encoder.append(&blobs_bundle);
                encoder.append(&execution_requests);
                encoder.append(&signature);
            }
            AlloySubmitBlockRequest::Electra(request) => {
                let SignedBidSubmissionV4 {
                    message,
                    execution_payload,
                    blobs_bundle,
                    execution_requests,
                    signature,
                } = request;
                encoder.append(&message);
                encoder.append(&execution_payload);
                encoder.append(&blobs_bundle);
                encoder.append(&execution_requests);
                encoder.append(&signature);
            }
            AlloySubmitBlockRequest::Deneb(request) => {
                let SignedBidSubmissionV3 {
                    message,
                    execution_payload,
                    blobs_bundle,
                    signature,
                } = request;
                encoder.append(&message);
                encoder.append(&execution_payload);
                encoder.append(&blobs_bundle);
                encoder.append(&signature);
            }
            AlloySubmitBlockRequest::Capella(request) => {
                let SignedBidSubmissionV2 {
                    message,
                    execution_payload,
                    signature,
                } = request;
                encoder.append(&message);
                encoder.append(&execution_payload);
                encoder.append(&signature);
            }
        };
        if let Some(adjustment) = &self.adjustment_data {
            encoder.append(&adjustment);
        }

        encoder.finalize();
    }

    fn ssz_bytes_len(&self) -> usize {
        let mut len = <AlloySubmitBlockRequest as ssz::Encode>::ssz_bytes_len(&self.request);
        if let Some(adjustment) = &self.adjustment_data {
            len += <BidAdjustmentDataV1 as ssz::Encode>::ssz_bytes_len(adjustment);
        }
        len
    }
}

impl ssz::Decode for SubmitBlockRequest {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    // A naive implementation of decoding where we attempt to decode each variant with or without adjustments.
    // Optimize this if it becomes latency sensitive.
    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        // Fulu (with adjustments)
        if let Ok(request) = ssz_helpers::FuluSubmitBlockRequestSszHelper::from_ssz_bytes(bytes) {
            return Ok(request.into());
        }

        // Electra (with adjustments)
        if let Ok(request) = ssz_helpers::ElectraSubmitBlockRequestSszHelper::from_ssz_bytes(bytes)
        {
            return Ok(request.into());
        }

        // Deneb (with adjustments)
        if let Ok(request) = ssz_helpers::DenebSubmitBlockRequestSszHelper::from_ssz_bytes(bytes) {
            return Ok(request.into());
        }

        // Capella (with adjustments)
        if let Ok(request) = ssz_helpers::CapellaSubmitBlockRequestSszHelper::from_ssz_bytes(bytes)
        {
            return Ok(request.into());
        }

        // Any (no adjustments)
        let request = Arc::new(AlloySubmitBlockRequest::from_ssz_bytes(bytes)?);
        Ok(Self {
            request,
            adjustment_data: None,
        })
    }
}

impl SubmitBlockRequest {
    pub fn signature(&self) -> BlsSignature {
        match self.request.as_ref() {
            AlloySubmitBlockRequest::Capella(req) => req.signature,
            AlloySubmitBlockRequest::Deneb(req) => req.signature,
            AlloySubmitBlockRequest::Electra(req) => req.signature,
            AlloySubmitBlockRequest::Fulu(req) => req.signature,
        }
    }

    pub fn execution_payload_v1(&self) -> &ExecutionPayloadV1 {
        match self.request.as_ref() {
            AlloySubmitBlockRequest::Capella(req) => &req.execution_payload.payload_inner,
            AlloySubmitBlockRequest::Deneb(req) => {
                &req.execution_payload.payload_inner.payload_inner
            }
            AlloySubmitBlockRequest::Electra(req) => {
                &req.execution_payload.payload_inner.payload_inner
            }
            AlloySubmitBlockRequest::Fulu(req) => {
                &req.execution_payload.payload_inner.payload_inner
            }
        }
    }

    pub fn execution_payload_v2(&self) -> &ExecutionPayloadV2 {
        match self.request.as_ref() {
            AlloySubmitBlockRequest::Capella(req) => &req.execution_payload,
            AlloySubmitBlockRequest::Deneb(req) => &req.execution_payload.payload_inner,
            AlloySubmitBlockRequest::Electra(req) => &req.execution_payload.payload_inner,
            AlloySubmitBlockRequest::Fulu(req) => &req.execution_payload.payload_inner,
        }
    }

    pub fn execution_payload_v3(&self) -> Option<&ExecutionPayloadV3> {
        match self.request.as_ref() {
            AlloySubmitBlockRequest::Capella(_) => None,
            AlloySubmitBlockRequest::Deneb(req) => Some(&req.execution_payload),
            AlloySubmitBlockRequest::Electra(req) => Some(&req.execution_payload),
            AlloySubmitBlockRequest::Fulu(req) => Some(&req.execution_payload),
        }
    }

    /// Returns `true` if block has adjustment data.
    pub fn has_adjustment_data(&self) -> bool {
        self.adjustment_data.is_some()
    }

    /// Set the bid adjustment data on the request.
    pub fn set_adjustment_data(&mut self, data: BidAdjustmentDataV1) {
        self.adjustment_data = Some(data);
    }

    /// Remove adjustment data from the bid.
    pub fn remove_adjustment_data(&mut self) {
        self.adjustment_data.take();
    }

    /// Remove adjustment data from the bid and return it.
    pub fn without_adjustment_data(mut self) -> Self {
        self.remove_adjustment_data();
        self
    }
}

/// Signed bid submission that is serialized without blobs bundle.
#[derive(Debug)]
pub struct SubmitBlockRequestNoBlobs<'a>(pub &'a SubmitBlockRequest);

impl serde::Serialize for SubmitBlockRequestNoBlobs<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0.request.as_ref() {
            AlloySubmitBlockRequest::Capella(v2) => v2.serialize(serializer),
            AlloySubmitBlockRequest::Deneb(v3) => {
                #[derive(serde::Serialize)]
                struct SignedBidSubmissionV3Ref<'a> {
                    message: &'a BidTrace,
                    #[serde(with = "alloy_rpc_types_beacon::payload::beacon_payload_v3")]
                    execution_payload: &'a ExecutionPayloadV3,
                    blobs_bundle: &'a BlobsBundleV1,
                    signature: &'a BlsSignature,
                }

                SignedBidSubmissionV3Ref {
                    message: &v3.message,
                    execution_payload: &v3.execution_payload,
                    blobs_bundle: &BlobsBundleV1::new([]), // override blobs bundle with empty one
                    signature: &v3.signature,
                }
                .serialize(serializer)
            }
            AlloySubmitBlockRequest::Electra(v4) => {
                #[derive(serde::Serialize)]
                struct SignedBidSubmissionV4Ref<'a> {
                    message: &'a BidTrace,
                    #[serde(with = "alloy_rpc_types_beacon::payload::beacon_payload_v3")]
                    execution_payload: &'a ExecutionPayloadV3,
                    blobs_bundle: &'a BlobsBundleV1,
                    execution_requests: &'a ExecutionRequestsV4,
                    signature: &'a BlsSignature,
                }

                SignedBidSubmissionV4Ref {
                    message: &v4.message,
                    execution_payload: &v4.execution_payload,
                    blobs_bundle: &BlobsBundleV1::new([]), // override blobs bundle with empty one
                    signature: &v4.signature,
                    execution_requests: &v4.execution_requests,
                }
                .serialize(serializer)
            }
            AlloySubmitBlockRequest::Fulu(v5) => {
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

mod ssz_helpers {
    use super::*;

    #[derive(ssz_derive::Decode)]
    pub(crate) struct CapellaSubmitBlockRequestSszHelper {
        message: BidTrace,
        execution_payload: ExecutionPayloadV2,
        signature: BlsSignature,
        adjustment_data: BidAdjustmentDataV1,
    }

    impl From<CapellaSubmitBlockRequestSszHelper> for SubmitBlockRequest {
        fn from(value: CapellaSubmitBlockRequestSszHelper) -> Self {
            let CapellaSubmitBlockRequestSszHelper {
                message,
                execution_payload,
                signature,
                adjustment_data,
            } = value;
            Self {
                request: Arc::new(AlloySubmitBlockRequest::Capella(SignedBidSubmissionV2 {
                    message,
                    execution_payload,
                    signature,
                })),
                adjustment_data: Some(adjustment_data),
            }
        }
    }

    #[derive(ssz_derive::Decode)]
    pub(crate) struct DenebSubmitBlockRequestSszHelper {
        message: BidTrace,
        execution_payload: ExecutionPayloadV3,
        blobs_bundle: BlobsBundleV1,
        signature: BlsSignature,
        adjustment_data: BidAdjustmentDataV1,
    }

    impl From<DenebSubmitBlockRequestSszHelper> for SubmitBlockRequest {
        fn from(value: DenebSubmitBlockRequestSszHelper) -> Self {
            let DenebSubmitBlockRequestSszHelper {
                message,
                execution_payload,
                blobs_bundle,
                signature,
                adjustment_data,
            } = value;
            Self {
                request: Arc::new(AlloySubmitBlockRequest::Deneb(SignedBidSubmissionV3 {
                    message,
                    execution_payload,
                    blobs_bundle,
                    signature,
                })),
                adjustment_data: Some(adjustment_data),
            }
        }
    }

    #[derive(ssz_derive::Decode)]
    pub(crate) struct ElectraSubmitBlockRequestSszHelper {
        message: BidTrace,
        execution_payload: ExecutionPayloadV3,
        blobs_bundle: BlobsBundleV1,
        execution_requests: ExecutionRequestsV4,
        signature: BlsSignature,
        adjustment_data: BidAdjustmentDataV1,
    }

    impl From<ElectraSubmitBlockRequestSszHelper> for SubmitBlockRequest {
        fn from(value: ElectraSubmitBlockRequestSszHelper) -> Self {
            let ElectraSubmitBlockRequestSszHelper {
                message,
                execution_payload,
                blobs_bundle,
                execution_requests,
                signature,
                adjustment_data,
            } = value;
            Self {
                request: Arc::new(AlloySubmitBlockRequest::Electra(SignedBidSubmissionV4 {
                    message,
                    execution_payload,
                    blobs_bundle,
                    execution_requests,
                    signature,
                })),
                adjustment_data: Some(adjustment_data),
            }
        }
    }

    #[derive(ssz_derive::Decode)]
    pub(crate) struct FuluSubmitBlockRequestSszHelper {
        message: BidTrace,
        execution_payload: ExecutionPayloadV3,
        blobs_bundle: BlobsBundleV2,
        execution_requests: ExecutionRequestsV4,
        signature: BlsSignature,
        adjustment_data: BidAdjustmentDataV1,
    }

    impl From<FuluSubmitBlockRequestSszHelper> for SubmitBlockRequest {
        fn from(value: FuluSubmitBlockRequestSszHelper) -> Self {
            let FuluSubmitBlockRequestSszHelper {
                message,
                execution_payload,
                blobs_bundle,
                execution_requests,
                signature,
                adjustment_data,
            } = value;
            Self {
                request: Arc::new(AlloySubmitBlockRequest::Fulu(SignedBidSubmissionV5 {
                    message,
                    execution_payload,
                    blobs_bundle,
                    execution_requests,
                    signature,
                })),
                adjustment_data: Some(adjustment_data),
            }
        }
    }
}
