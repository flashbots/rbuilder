use alloy_eips::{
    eip2718::Encodable2718,
    eip4844::BlobTransactionSidecar,
    eip7594::{BlobTransactionSidecarEip7594, BlobTransactionSidecarVariant},
    eip7685::Requests,
};
use alloy_primitives::{Bytes, U256};
use alloy_rpc_types::Withdrawals;
use alloy_rpc_types_beacon::relay::SubmitBlockRequest as AlloySubmitBlockRequest;
use alloy_rpc_types_beacon::{
    events::PayloadAttributesData,
    relay::{
        BidTrace, SignedBidSubmissionV2, SignedBidSubmissionV3, SignedBidSubmissionV4,
        SignedBidSubmissionV5,
    },
    requests::ExecutionRequestsV4,
    BlsSignature,
};
use alloy_rpc_types_engine::{
    BlobsBundleV1, BlobsBundleV2, ExecutionPayload, ExecutionPayloadV1, ExecutionPayloadV2,
    ExecutionPayloadV3,
};
use reth_chainspec::{ChainSpec, EthereumHardforks};
use reth_primitives::SealedBlock;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct SignedBuiltBlock {
    pub message: BidTrace,
    pub signature: BlsSignature,
    pub execution_payload: ExecutionPayload,
    pub blob_sidecars: Vec<Arc<BlobTransactionSidecarVariant>>,
    pub execution_requests: Vec<Bytes>,
}

impl SignedBuiltBlock {
    /// Convert the signed block into [`SubmitBlockRequest`](`alloy_rpc_types_beacon::relay::SubmitBlockRequest`).
    pub fn into_request(self, chain_spec: &ChainSpec) -> eyre::Result<AlloySubmitBlockRequest> {
        match self.execution_payload {
            ExecutionPayload::V1(_v1) => {
                eyre::bail!("v1 payloads are not supported");
            }
            ExecutionPayload::V2(v2) => {
                Ok(AlloySubmitBlockRequest::Capella(SignedBidSubmissionV2 {
                    message: self.message,
                    execution_payload: v2,
                    signature: self.signature,
                }))
            }
            ExecutionPayload::V3(v3) => {
                if chain_spec.is_osaka_active_at_timestamp(v3.timestamp()) {
                    let execution_requests = ExecutionRequestsV4::try_from(Requests::new(
                        self.execution_requests.to_vec(),
                    ))?;
                    let blobs_bundle_v2 = marshall_txs_blobs_sidecars_v2(&self.blob_sidecars);
                    return Ok(AlloySubmitBlockRequest::Fulu(SignedBidSubmissionV5 {
                        message: self.message,
                        execution_payload: v3,
                        blobs_bundle: blobs_bundle_v2,
                        signature: self.signature,
                        execution_requests,
                    }));
                }

                let blobs_bundle = marshal_txs_blobs_sidecars(&self.blob_sidecars);
                if chain_spec.is_prague_active_at_timestamp(v3.timestamp()) {
                    let execution_requests = ExecutionRequestsV4::try_from(Requests::new(
                        self.execution_requests.to_vec(),
                    ))?;
                    return Ok(AlloySubmitBlockRequest::Electra(SignedBidSubmissionV4 {
                        message: self.message,
                        execution_payload: v3,
                        blobs_bundle,
                        signature: self.signature,
                        execution_requests,
                    }));
                }

                Ok(AlloySubmitBlockRequest::Deneb(SignedBidSubmissionV3 {
                    message: self.message,
                    execution_payload: v3,
                    blobs_bundle,
                    signature: self.signature,
                }))
            }
        }
    }
}

fn marshal_txs_blobs_sidecars(
    txs_blobs_sidecars: &[Arc<BlobTransactionSidecarVariant>],
) -> BlobsBundleV1 {
    // Instead of collecting Arc<BlobTransactionSidecar>, just collect references to the inner struct.
    let eip4844_sidecars: Vec<&BlobTransactionSidecar> = txs_blobs_sidecars
        .iter()
        .filter_map(|blob| blob.as_ref().as_eip4844())
        .collect();

    // Now flatten the fields, only cloning the inner data, not the whole struct or Arc.
    let commitments = eip4844_sidecars
        .iter()
        .flat_map(|t| t.commitments.iter().cloned())
        .collect();

    let proofs = eip4844_sidecars
        .iter()
        .flat_map(|t| t.proofs.iter().cloned())
        .collect();

    let blobs = eip4844_sidecars
        .iter()
        .flat_map(|t| t.blobs.iter().cloned())
        .collect();

    BlobsBundleV1 {
        commitments,
        proofs,
        blobs,
    }
}

fn marshall_txs_blobs_sidecars_v2(
    txs_blobs_sidecars: &[Arc<BlobTransactionSidecarVariant>],
) -> BlobsBundleV2 {
    // Instead of collecting Arc<BlobTransactionSidecarEip7594>, just collect references to the inner struct.
    let eip7594_sidecars: Vec<&BlobTransactionSidecarEip7594> = txs_blobs_sidecars
        .iter()
        .filter_map(|blob| blob.as_ref().as_eip7594())
        .collect();

    // Now flatten the fields, only cloning the inner data, not the whole struct or Arc.
    let commitments = eip7594_sidecars
        .iter()
        .flat_map(|t| t.commitments.iter().cloned())
        .collect();

    let proofs = eip7594_sidecars
        .iter()
        .flat_map(|t| t.cell_proofs.iter().cloned())
        .collect();

    let blobs = eip7594_sidecars
        .iter()
        .flat_map(|t| t.blobs.iter().cloned())
        .collect();

    BlobsBundleV2 {
        commitments,
        proofs,
        blobs,
    }
}

/// Utility function to convert built block to execution payload.
pub fn block_to_execution_payload(
    chain_spec: &ChainSpec,
    attrs: &PayloadAttributesData,
    sealed_block: &SealedBlock,
) -> ExecutionPayload {
    let transactions = sealed_block
        .body()
        .transactions
        .iter()
        .map(|tx| tx.encoded_2718().into())
        .collect();
    let payload_v1 = ExecutionPayloadV1 {
        parent_hash: sealed_block.parent_hash,
        fee_recipient: sealed_block.beneficiary,
        state_root: sealed_block.state_root,
        receipts_root: sealed_block.receipts_root,
        logs_bloom: sealed_block.logs_bloom,
        prev_randao: attrs.payload_attributes.prev_randao,
        block_number: sealed_block.number,
        gas_limit: sealed_block.gas_limit,
        gas_used: sealed_block.gas_used,
        timestamp: sealed_block.timestamp,
        extra_data: sealed_block.extra_data.clone(),
        base_fee_per_gas: U256::from(sealed_block.base_fee_per_gas.unwrap_or_default()),
        block_hash: sealed_block.hash(),
        transactions,
    };
    let payload_v2 = ExecutionPayloadV2 {
        payload_inner: payload_v1,
        withdrawals: sealed_block
            .body()
            .withdrawals
            .clone()
            .map(Withdrawals::into_inner)
            .unwrap_or_default(),
    };

    if chain_spec.is_cancun_active_at_timestamp(sealed_block.timestamp) {
        ExecutionPayload::V3(ExecutionPayloadV3 {
            payload_inner: payload_v2,
            blob_gas_used: sealed_block
                .blob_gas_used
                .expect("deneb block does not have blob gas used"),
            excess_blob_gas: sealed_block
                .excess_blob_gas
                .expect("deneb block does not have excess blob gas"),
        })
    } else {
        ExecutionPayload::V2(payload_v2)
    }
}
