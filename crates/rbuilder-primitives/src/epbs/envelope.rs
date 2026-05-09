//! ExecutionPayloadEnvelope types for EPBS.
//!
//! These types represent the full execution payload that the builder reveals
//! after their bid is included in a beacon block.
//! See: https://github.com/ethereum/consensus-specs/blob/master/specs/gloas/builder.md

use alloy_eips::eip7594::BlobTransactionSidecarVariant;
use alloy_primitives::{Bytes, B256};
use alloy_rpc_types_beacon::BlsSignature;
use alloy_rpc_types_engine::ExecutionPayloadV3;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use std::sync::Arc;

/// ExecutionPayloadEnvelope contains the full execution payload and associated data.
///
/// This is revealed by the builder after their SignedExecutionPayloadBid is included
/// in a beacon block. The envelope is broadcast on the `execution_payload` P2P topic.
///
/// From consensus-specs/specs/gloas/beacon-chain.md:

#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPayloadEnvelope {
    /// The full execution payload.
    /// TODO: This should be the Gloas-specific ExecutionPayload when available in Alloy.
    pub payload: ExecutionPayloadV3,
    /// Execution requests (deposits, withdrawals, consolidations).
    /// TODO: Use proper ExecutionRequests type from Alloy when available.
    pub execution_requests: ExecutionRequests,
    /// Validator index of the builder.
    #[serde_as(as = "DisplayFromStr")]
    pub builder_index: u64,
    /// Hash tree root of the beacon block that included this builder's bid.
    pub beacon_block_root: B256,
    /// Slot of the beacon block.
    #[serde_as(as = "DisplayFromStr")]
    pub slot: u64,
    /// Blob KZG commitments for the payload.
    #[serde(default)]
    pub blob_kzg_commitments: Vec<Bytes>,
    /// State root after applying the execution payload.
    pub state_root: B256,
}

/// Placeholder for ExecutionRequests until available in Alloy.
/// TODO: Replace with alloy_rpc_types_beacon::requests::ExecutionRequestsV4 or equivalent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRequests {
    /// Deposit requests from the execution layer.
    #[serde(default)]
    pub deposits: Vec<Bytes>,
    /// Withdrawal requests from the execution layer.
    #[serde(default)]
    pub withdrawals: Vec<Bytes>,
    /// Consolidation requests from the execution layer.
    #[serde(default)]
    pub consolidations: Vec<Bytes>,
}

/// SignedExecutionPayloadEnvelope is the envelope signed by the builder.
///
/// From consensus-specs/specs/gloas/beacon-chain.md:
/// ```python
/// class SignedExecutionPayloadEnvelope(Container):
///     message: ExecutionPayloadEnvelope
///     signature: BLSSignature
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedExecutionPayloadEnvelope {
    /// The execution payload envelope message.
    pub message: ExecutionPayloadEnvelope,
    /// BLS signature over the envelope using the builder's validator key.
    pub signature: BlsSignature,
}

/// Cached payload data stored by the builder after creating a bid.
///
/// When a builder creates an ExecutionPayloadBid, they must store the full
/// payload data so they can reveal it when/if their bid is accepted.
#[derive(Debug, Clone)]
pub struct CachedPayloadData {
    /// The signed bid that was broadcast/returned.
    pub bid: super::SignedExecutionPayloadBid,
    /// The full execution payload (to be revealed later).
    pub payload: ExecutionPayloadV3,
    /// Execution requests associated with the payload.
    pub execution_requests: ExecutionRequests,
    /// Blob KZG commitments.
    pub blob_kzg_commitments: Vec<Bytes>,
    /// Reference to the original blob sidecars from the built block. Held by
    /// `Arc` so cache inserts are pointer bumps, not 128KB-per-blob copies.
    /// Use `blobs()` and `cell_proofs()` to materialize the wire-format
    /// `Vec<Bytes>` for the envelope-publish API.
    pub sidecars: Vec<Arc<BlobTransactionSidecarVariant>>,
    /// Timestamp when this cache entry was created.
    pub created_at: std::time::Instant,
}

impl CachedPayloadData {
    /// Creates a new cached payload entry.
    pub fn new(
        bid: super::SignedExecutionPayloadBid,
        payload: ExecutionPayloadV3,
        execution_requests: ExecutionRequests,
        blob_kzg_commitments: Vec<Bytes>,
        sidecars: Vec<Arc<BlobTransactionSidecarVariant>>,
    ) -> Self {
        Self {
            bid,
            payload,
            execution_requests,
            blob_kzg_commitments,
            sidecars,
            created_at: std::time::Instant::now(),
        }
    }

    /// Build the envelope from cached data and the beacon block info.
    pub fn build_envelope(
        &self,
        beacon_block_root: B256,
        state_root: B256,
    ) -> ExecutionPayloadEnvelope {
        ExecutionPayloadEnvelope {
            payload: self.payload.clone(),
            execution_requests: self.execution_requests.clone(),
            builder_index: self.bid.message.builder_index,
            beacon_block_root,
            slot: self.bid.message.slot,
            blob_kzg_commitments: self.blob_kzg_commitments.clone(),
            state_root,
        }
    }

    /// Materialize raw blob bytes from the sidecars in publish api order
    pub fn blobs(&self) -> Vec<Bytes> {
        let mut out = Vec::new();
        for sidecar in &self.sidecars {
            if let BlobTransactionSidecarVariant::Eip7594(s) = sidecar.as_ref() {
                for blob in &s.blobs {
                    out.push(Bytes::copy_from_slice(blob.as_ref()));
                }
            }
        }
        out
    }

    /// Materialize the flat list of EIP-7594 cell proofs in publish api order
    pub fn cell_proofs(&self) -> Vec<Bytes> {
        let mut out = Vec::new();
        for sidecar in &self.sidecars {
            if let BlobTransactionSidecarVariant::Eip7594(s) = sidecar.as_ref() {
                for proof in &s.cell_proofs {
                    out.push(Bytes::copy_from_slice(proof.as_slice()));
                }
            }
        }
        out
    }
}
