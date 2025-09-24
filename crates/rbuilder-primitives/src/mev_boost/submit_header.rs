use crate::mev_boost::adjustment::BidAdjustmentDataV2;
use alloy_primitives::{Address, Bloom, Bytes, B256, U256};
use alloy_rpc_types_beacon::{relay::BidTrace, requests::ExecutionRequestsV4, BlsSignature};
use serde_with::{serde_as, DisplayFromStr};

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
pub struct HeaderSubmissionV3 {
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
    pub message: HeaderSubmissionElectra,
    /// Builder signature.
    pub signature: BlsSignature,
}

/// Electra header submission.
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
    pub adjustment_data: BidAdjustmentDataV2,
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
