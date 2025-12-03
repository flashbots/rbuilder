use crate::mev_boost::{
    adjustment::BidAdjustmentDataV2,
    ssz_roots::{calculate_transactions_root_ssz, calculate_withdrawals_root_ssz},
    BidMetadata,
};
use alloy_primitives::{Address, Bloom, Bytes, B256, U256};
use alloy_rpc_types_beacon::{relay::BidTrace, requests::ExecutionRequestsV4, BlsSignature};
use alloy_rpc_types_engine::ExecutionPayloadV3;
use serde_with::{serde_as, DisplayFromStr};

/// Optimistic V3 bid submission with metadata.
#[derive(Clone, Debug)]
pub struct SubmitHeaderRequestWithMetadata {
    /// Header submission.
    pub submission: SubmitHeaderRequest,
    /// Bid metadata.
    pub metadata: BidMetadata,
}

/// Optimistic V3 bid submission.
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
pub struct SubmitHeaderRequest {
    /// URL pointing to the builder's server endpoint for retrieving
    /// the full block payload if this header is selected.
    pub url: Vec<u8>,
    /// The number of transactions in the block.
    pub tx_count: u32,
    /// The signed header data. This is the same structure used by
    /// the Optimistic V2 'SignedHeaderSubmission'.
    pub submission: SignedHeaderSubmission,
}

/// Signed header submission.
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
pub struct SignedHeaderSubmission {
    /// Electra header submission.
    pub message: HeaderSubmission,
    /// Builder signature.
    pub signature: BlsSignature,
}

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
#[ssz(enum_behaviour = "transparent")]
pub enum HeaderSubmission {
    Electra(HeaderSubmissionElectra),
    Fulu(HeaderSubmissionElectra),
}

/// Electra header submission.
#[derive(PartialEq, Eq, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HeaderSubmissionElectra {
    /// Bid trace.
    pub bid_trace: BidTrace,
    /// Execution payload header.
    pub execution_payload_header: ExecutionPayloadHeaderElectra,
    /// Execution requests.
    pub execution_requests: ExecutionRequestsV4,
    /// Blob KZG commitments.
    pub commitments: Vec<alloy_consensus::Bytes48>,
    /// Bid adjustment data V2.
    pub adjustment_data: Option<BidAdjustmentDataV2>,
}

impl ssz::Encode for HeaderSubmissionElectra {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        let mut offset = <BidTrace as ssz::Encode>::ssz_fixed_len()
            + <ExecutionPayloadHeaderElectra as ssz::Encode>::ssz_fixed_len()
            + <ExecutionRequestsV4 as ssz::Encode>::ssz_fixed_len()
            + <Vec<alloy_consensus::Bytes48> as ssz::Encode>::ssz_fixed_len();
        if self.adjustment_data.is_some() {
            offset += <BidAdjustmentDataV2 as ssz::Encode>::ssz_fixed_len();
        }

        let mut encoder = ssz::SszEncoder::container(buf, offset);

        encoder.append(&self.bid_trace);
        encoder.append(&self.execution_payload_header);
        encoder.append(&self.execution_requests);
        encoder.append(&self.commitments);
        if let Some(adjustment) = &self.adjustment_data {
            encoder.append(&adjustment);
        }

        encoder.finalize();
    }

    fn ssz_bytes_len(&self) -> usize {
        let mut len = <BidTrace as ssz::Encode>::ssz_bytes_len(&self.bid_trace)
            + <ExecutionPayloadHeaderElectra as ssz::Encode>::ssz_bytes_len(
                &self.execution_payload_header,
            )
            + <ExecutionRequestsV4 as ssz::Encode>::ssz_bytes_len(&self.execution_requests)
            + <Vec<alloy_consensus::Bytes48> as ssz::Encode>::ssz_bytes_len(&self.commitments);
        if let Some(adjustment) = &self.adjustment_data {
            len += <BidAdjustmentDataV2 as ssz::Encode>::ssz_bytes_len(adjustment);
        }
        len
    }
}

impl ssz::Decode for HeaderSubmissionElectra {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        #[derive(ssz_derive::Decode)]
        struct HeaderSubmissionElectraSszHelper {
            bid_trace: BidTrace,
            execution_payload_header: ExecutionPayloadHeaderElectra,
            execution_requests: ExecutionRequestsV4,
            commitments: Vec<alloy_consensus::Bytes48>,
            adjustment_data: BidAdjustmentDataV2,
        }

        #[derive(ssz_derive::Decode)]
        struct HeaderSubmissionElectraNoAdjustmentsSszHelper {
            bid_trace: BidTrace,
            execution_payload_header: ExecutionPayloadHeaderElectra,
            execution_requests: ExecutionRequestsV4,
            commitments: Vec<alloy_consensus::Bytes48>,
        }

        if let Ok(submission) = HeaderSubmissionElectraSszHelper::from_ssz_bytes(bytes) {
            let HeaderSubmissionElectraSszHelper {
                bid_trace,
                execution_payload_header,
                execution_requests,
                commitments,
                adjustment_data,
            } = submission;
            Ok(Self {
                bid_trace,
                execution_payload_header,
                execution_requests,
                commitments,
                adjustment_data: Some(adjustment_data),
            })
        } else {
            let HeaderSubmissionElectraNoAdjustmentsSszHelper {
                bid_trace,
                execution_payload_header,
                execution_requests,
                commitments,
            } = HeaderSubmissionElectraNoAdjustmentsSszHelper::from_ssz_bytes(bytes)?;
            Ok(Self {
                bid_trace,
                execution_payload_header,
                execution_requests,
                commitments,
                adjustment_data: None,
            })
        }
    }
}

/// Electra execution payload header.
#[serde_as]
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
pub struct ExecutionPayloadHeaderElectra {
    /// The parent hash of the execution payload.
    pub parent_hash: B256,
    /// The fee recipient address of the execution payload.
    pub fee_recipient: Address,
    /// The state root of the execution payload.
    pub state_root: B256,
    /// The receipts root of the execution payload.
    pub receipts_root: B256,
    /// The logs bloom filter of the execution payload.
    pub logs_bloom: Bloom,
    /// The previous Randao value of the execution payload.
    pub prev_randao: B256,
    /// The block number of the execution payload, represented as a string.
    #[serde_as(as = "DisplayFromStr")]
    pub block_number: u64,
    /// The gas limit of the execution payload, represented as a `u64`.
    #[serde_as(as = "DisplayFromStr")]
    pub gas_limit: u64,
    /// The gas used by the execution payload, represented as a `u64`.
    #[serde_as(as = "DisplayFromStr")]
    pub gas_used: u64,
    /// The timestamp of the execution payload, represented as a `u64`.
    #[serde_as(as = "DisplayFromStr")]
    pub timestamp: u64,
    /// The extra data of the execution payload.
    pub extra_data: Bytes,
    /// The base fee per gas of the execution payload, represented as a `U256`.
    #[serde_as(as = "DisplayFromStr")]
    pub base_fee_per_gas: U256,
    /// The block hash of the execution payload.
    pub block_hash: B256,
    /// The SSZ transactions root of the execution payload.
    pub transactions_root: B256,
    /// The SSZ withdrawals root of the execution payload.
    pub withdrawals_root: B256,
    /// The total amount of blob gas consumed by the transactions within the block, added in
    /// EIP-4844.
    #[serde_as(as = "DisplayFromStr")]
    pub blob_gas_used: u64,
    /// A running total of blob gas consumed in excess of the target, prior to the block. Blocks
    /// with above-target blob gas consumption increase this value, blocks with below-target blob
    /// gas consumption decrease it (bounded at 0). This was added in EIP-4844.
    #[serde_as(as = "DisplayFromStr")]
    pub excess_blob_gas: u64,
}

impl From<&ExecutionPayloadV3> for ExecutionPayloadHeaderElectra {
    fn from(v3: &ExecutionPayloadV3) -> Self {
        let v2 = &v3.payload_inner;
        let v1 = &v2.payload_inner;
        ExecutionPayloadHeaderElectra {
            parent_hash: v1.parent_hash,
            fee_recipient: v1.fee_recipient,
            state_root: v1.state_root,
            receipts_root: v1.receipts_root,
            logs_bloom: v1.logs_bloom,
            prev_randao: v1.prev_randao,
            block_number: v1.block_number,
            gas_limit: v1.gas_limit,
            gas_used: v1.gas_used,
            timestamp: v1.timestamp,
            extra_data: v1.extra_data.clone(),
            base_fee_per_gas: v1.base_fee_per_gas,
            block_hash: v1.block_hash,
            transactions_root: calculate_transactions_root_ssz(&v1.transactions),
            withdrawals_root: calculate_withdrawals_root_ssz(&v2.withdrawals),
            blob_gas_used: v3.blob_gas_used,
            excess_blob_gas: v3.excess_blob_gas,
        }
    }
}
