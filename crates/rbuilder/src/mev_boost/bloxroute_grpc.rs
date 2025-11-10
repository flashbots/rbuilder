use alloy_eips::{
    eip4895::Withdrawal, eip6110::DepositRequest, eip7002::WithdrawalRequest,
    eip7251::ConsolidationRequest,
};
use alloy_rpc_types_beacon::{
    relay::{
        BidTrace, SignedBidSubmissionV2, SignedBidSubmissionV3, SignedBidSubmissionV4,
        SignedBidSubmissionV5, SubmitBlockRequest as AlloySubmitBlockRequest,
    },
    requests::ExecutionRequestsV4,
};
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
use rbuilder_primitives::mev_boost::SubmitBlockRequest;
use std::sync::Arc;

/// Bloxroute gRPC types.
pub mod types {
    tonic::include_proto!("bloxroute");
}

/// Data version corresponding to CL harforks.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
#[repr(u8)]
pub enum DataVersion {
    /// Bellatrix CL hardfork.
    Bellatrix = 3,
    /// Capella CL hardfork.
    Capella = 4,
    /// Deneb CL hardfork.
    Deneb = 5,
    /// Electra CL hardfork.
    Electra = 6,
    /// Fulu CL hardfork.
    Fulu = 7,
}

/// gRPC relay client type.
pub type GrpcRelayClient =
    Arc<tokio::sync::Mutex<types::relay_client::RelayClient<tonic::transport::Channel>>>;

impl From<&SubmitBlockRequest> for types::SubmitBlockRequest {
    fn from(value: &SubmitBlockRequest) -> Self {
        let (version, execution_payload, bid_trace, signature, blobs_bundle, execution_requests) =
            match value.request.as_ref() {
                AlloySubmitBlockRequest::Capella(request) => {
                    let SignedBidSubmissionV2 {
                        execution_payload,
                        message,
                        signature,
                    } = request;

                    let version = DataVersion::Capella;
                    let execution_payload = types::ExecutionPayload::from_v2(execution_payload);
                    let bid_trace = types::BidTrace::new(message, None, None);
                    (version, execution_payload, bid_trace, signature, None, None)
                }
                AlloySubmitBlockRequest::Deneb(request) => {
                    let SignedBidSubmissionV3 {
                        execution_payload,
                        message,
                        signature,
                        blobs_bundle,
                    } = request;

                    let version = DataVersion::Deneb;
                    let execution_payload = types::ExecutionPayload::from_v3(execution_payload);
                    let bid_trace = types::BidTrace::new(
                        message,
                        Some(execution_payload.blob_gas_used),
                        Some(execution_payload.excess_blob_gas),
                    );
                    (
                        version,
                        execution_payload,
                        bid_trace,
                        signature,
                        Some(types::BlobsBundle::from(blobs_bundle)),
                        None,
                    )
                }
                AlloySubmitBlockRequest::Electra(request) => {
                    let SignedBidSubmissionV4 {
                        execution_payload,
                        message,
                        signature,
                        blobs_bundle,
                        execution_requests,
                    } = request;

                    let version = DataVersion::Electra;
                    let execution_payload = types::ExecutionPayload::from_v3(execution_payload);
                    let bid_trace = types::BidTrace::new(
                        message,
                        Some(execution_payload.blob_gas_used),
                        Some(execution_payload.excess_blob_gas),
                    );
                    (
                        version,
                        execution_payload,
                        bid_trace,
                        signature,
                        Some(types::BlobsBundle::from(blobs_bundle)),
                        Some(execution_requests),
                    )
                }
                AlloySubmitBlockRequest::Fulu(request) => {
                    let SignedBidSubmissionV5 {
                        execution_payload,
                        message,
                        signature,
                        blobs_bundle,
                        execution_requests,
                    } = request;

                    let version = DataVersion::Electra;
                    let execution_payload = types::ExecutionPayload::from_v3(execution_payload);
                    let bid_trace = types::BidTrace::new(
                        message,
                        Some(execution_payload.blob_gas_used),
                        Some(execution_payload.excess_blob_gas),
                    );
                    (
                        version,
                        execution_payload,
                        bid_trace,
                        signature,
                        Some(types::BlobsBundle::from(blobs_bundle)),
                        Some(execution_requests),
                    )
                }
            };
        let execution_requests = execution_requests.map(types::ExecutionRequests::from);
        let adjustment_data = value
            .adjustment_data
            .as_ref()
            .map(ssz::Encode::as_ssz_bytes)
            .unwrap_or_default();

        Self {
            version: version as u64,
            execution_payload: Some(execution_payload),
            bid_trace: Some(bid_trace),
            signature: signature.0.to_vec(),
            adjustment_data,
            execution_requests,
            auth_header: String::new(), // set in the metadata instead
            size_before: 0,
            get_payload_only: false,
            blobs_bundle,
            second_value_auction_eligible: false,
            proposer_mev_protect: false,
            skip_optimism: false,
            compliance_list: false,
            get_payload_only_builder_requested: false,
            get_payload_only_region_locked: false,
            get_payload_only_no_adjustment_data: false,
        }
    }
}

impl types::ExecutionPayload {
    /// Create bloxroute gRPC execution payload from [`ExecutionPayloadV1`].
    pub fn from_v1(payload: &ExecutionPayloadV1) -> Self {
        let ExecutionPayloadV1 {
            parent_hash,
            fee_recipient,
            state_root,
            receipts_root,
            logs_bloom,
            prev_randao,
            block_number,
            gas_limit,
            gas_used,
            timestamp,
            extra_data,
            base_fee_per_gas,
            block_hash,
            transactions,
        } = payload;

        let transactions = transactions
            .iter()
            .map(|t| types::CompressTx {
                short_id: 0,
                raw_data: t.to_vec(),
            })
            .collect();

        Self {
            parent_hash: parent_hash.0.to_vec(),
            state_root: state_root.0.to_vec(),
            receipts_root: receipts_root.0.to_vec(),
            logs_bloom: logs_bloom.0 .0.to_vec(),
            prev_randao: prev_randao.0.to_vec(),
            extra_data: extra_data.0.to_vec(),
            base_fee_per_gas: base_fee_per_gas.to_be_bytes_vec(),
            fee_recipient: fee_recipient.0.to_vec(),
            block_hash: block_hash.0.to_vec(),
            block_number: *block_number,
            gas_limit: *gas_limit,
            timestamp: *timestamp,
            gas_used: *gas_used,
            transactions,
            withdrawals: Vec::new(),
            blob_gas_used: 0,
            excess_blob_gas: 0,
        }
    }

    /// Create bloxroute gRPC execution payload from [`ExecutionPayloadV2`].
    pub fn from_v2(payload: &ExecutionPayloadV2) -> Self {
        let ExecutionPayloadV2 {
            payload_inner,
            withdrawals,
        } = payload;
        let mut payload = Self::from_v1(payload_inner);
        payload.withdrawals = withdrawals.iter().map(types::Withdrawal::from).collect();
        payload
    }

    /// Create bloxroute gRPC execution payload from [`ExecutionPayloadV3`].
    pub fn from_v3(payload: &ExecutionPayloadV3) -> Self {
        let ExecutionPayloadV3 {
            payload_inner,
            blob_gas_used,
            excess_blob_gas,
        } = payload;
        let mut payload = Self::from_v2(payload_inner);
        payload.blob_gas_used = *blob_gas_used;
        payload.excess_blob_gas = *excess_blob_gas;
        payload
    }
}

impl From<&Withdrawal> for types::Withdrawal {
    fn from(value: &Withdrawal) -> Self {
        Self {
            index: value.index,
            validator_index: value.validator_index,
            address: value.address.0.to_vec(),
            amount: value.amount,
        }
    }
}

impl types::BidTrace {
    /// Create new bloxroute gRPC bid trace from original bid trace and blob gas values.
    pub fn new(
        original: &BidTrace,
        blob_gas_used: Option<u64>,
        excess_blob_gas: Option<u64>,
    ) -> Self {
        Self {
            slot: original.slot,
            parent_hash: original.parent_hash.0.to_vec(),
            block_hash: original.block_hash.0.to_vec(),
            builder_pubkey: original.builder_pubkey.0.to_vec(),
            proposer_pubkey: original.proposer_pubkey.0.to_vec(),
            proposer_fee_recipient: original.proposer_fee_recipient.0.to_vec(),
            gas_limit: original.gas_limit,
            gas_used: original.gas_used,
            value: format!("{:#x}", original.value),
            blob_gas_used: blob_gas_used.unwrap_or_default(),
            excess_blob_gas: excess_blob_gas.unwrap_or_default(),
        }
    }
}

impl From<&alloy_rpc_types_engine::BlobsBundleV1> for types::BlobsBundle {
    fn from(value: &alloy_rpc_types_engine::BlobsBundleV1) -> Self {
        Self {
            commitments: value.commitments.iter().map(|c| c.0.to_vec()).collect(),
            proofs: value.proofs.iter().map(|p| p.0.to_vec()).collect(),
            blobs: value.blobs.iter().map(|b| b.0.to_vec()).collect(),
        }
    }
}

impl From<&alloy_rpc_types_engine::BlobsBundleV2> for types::BlobsBundle {
    fn from(value: &alloy_rpc_types_engine::BlobsBundleV2) -> Self {
        Self {
            commitments: value.commitments.iter().map(|c| c.0.to_vec()).collect(),
            proofs: value.proofs.iter().map(|p| p.0.to_vec()).collect(),
            blobs: value.blobs.iter().map(|b| b.0.to_vec()).collect(),
        }
    }
}

impl From<&ExecutionRequestsV4> for types::ExecutionRequests {
    fn from(value: &ExecutionRequestsV4) -> Self {
        Self {
            deposits: value
                .deposits
                .iter()
                .map(types::DepositRequest::from)
                .collect(),
            withdrawals: value
                .withdrawals
                .iter()
                .map(types::WithdrawalRequest::from)
                .collect(),
            consolidations: value
                .consolidations
                .iter()
                .map(types::ConsolidationRequest::from)
                .collect(),
        }
    }
}

impl From<&DepositRequest> for types::DepositRequest {
    fn from(value: &DepositRequest) -> Self {
        Self {
            pubkey: value.pubkey.0.to_vec(),
            withdrawal_credentials: value.withdrawal_credentials.0.to_vec(),
            amount: value.amount,
            signature: value.signature.0.to_vec(),
            index: value.index,
        }
    }
}

impl From<&WithdrawalRequest> for types::WithdrawalRequest {
    fn from(value: &WithdrawalRequest) -> Self {
        Self {
            source_address: value.source_address.0.to_vec(),
            validator_pubkey: value.validator_pubkey.0.to_vec(),
            amount: value.amount,
        }
    }
}

impl From<&ConsolidationRequest> for types::ConsolidationRequest {
    fn from(value: &ConsolidationRequest) -> Self {
        Self {
            source_address: value.source_address.0.to_vec(),
            source_pubkey: value.source_pubkey.0.to_vec(),
            target_pubkey: value.target_pubkey.0.to_vec(),
        }
    }
}
