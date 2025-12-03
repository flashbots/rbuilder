use alloy_consensus::{Blob, Bytes48};
use alloy_primitives::{Address, Bloom, Bytes, B256, U256};
use alloy_rpc_types_beacon::relay::SubmitBlockRequest as AlloySubmitBlockRequest;
use alloy_rpc_types_beacon::{
    events::PayloadAttributesData,
    relay::{BidTrace, SignedBidSubmissionV3},
    BlsPublicKey, BlsSignature,
};
use alloy_rpc_types_engine::{
    BlobsBundleV1, ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3,
};
use alloy_rpc_types_eth::Withdrawal;
use reth::rpc::types::engine::PayloadAttributes;

/// TestDataGenerator allows you to create unique test objects with unique content, it tries to use different numbers for every field it sets
#[derive(Default)]
pub struct TestDataGenerator {
    last_used_id: u64,
}

impl TestDataGenerator {
    pub fn create_deneb_submit_block_request(&mut self) -> AlloySubmitBlockRequest {
        AlloySubmitBlockRequest::Deneb(SignedBidSubmissionV3 {
            message: self.create_bid_trace(),
            execution_payload: self.create_deneb_payload(),
            blobs_bundle: self.create_txs_blobs_sidecars(),
            signature: self.create_signature(),
        })
    }

    pub fn create_txs_blobs_sidecars(&mut self) -> BlobsBundleV1 {
        BlobsBundleV1 {
            commitments: vec![self.create_commitment()],
            proofs: vec![self.create_proof()],
            blobs: vec![self.create_blob()],
        }
    }

    pub fn create_deneb_payload(&mut self) -> ExecutionPayloadV3 {
        ExecutionPayloadV3 {
            payload_inner: self.create_capella_payload(),
            blob_gas_used: self.create_u64(),
            excess_blob_gas: self.create_u64(),
        }
    }

    pub fn create_withdrawal(&mut self) -> Withdrawal {
        Withdrawal {
            index: self.create_u64(),
            validator_index: self.create_u64(),
            address: self.create_address(),
            amount: self.create_u64(),
        }
    }

    pub fn create_bid_trace(&mut self) -> BidTrace {
        BidTrace {
            slot: self.create_u64(),
            parent_hash: self.create_hash(),
            block_hash: self.create_hash(),
            builder_pubkey: self.create_key(),
            proposer_pubkey: self.create_key(),
            proposer_fee_recipient: self.create_address(),
            gas_limit: self.create_u64(),
            gas_used: self.create_u64(),
            value: self.create_u256(),
        }
    }

    pub fn create_capella_payload(&mut self) -> ExecutionPayloadV2 {
        ExecutionPayloadV2 {
            payload_inner: ExecutionPayloadV1 {
                parent_hash: self.create_hash(),
                fee_recipient: self.create_address(),
                state_root: self.create_hash(),
                receipts_root: self.create_hash(),
                logs_bloom: self.create_bloom(),
                prev_randao: self.create_hash(),
                block_number: self.create_u64(),
                gas_limit: self.create_u64(),
                gas_used: self.create_u64(),
                timestamp: self.create_u64(),
                extra_data: Bytes::from([self.create_u8(); 1]),
                base_fee_per_gas: self.create_u256(),
                block_hash: self.create_hash(),
                transactions: vec![Bytes::from([self.create_u8(); 1])],
            },
            withdrawals: vec![self.create_withdrawal()],
        }
    }

    pub fn create_payload_attribute_data(&mut self) -> PayloadAttributesData {
        PayloadAttributesData {
            proposal_slot: self.create_u64(),
            parent_block_root: self.create_hash(),
            parent_block_number: self.create_u64(),
            parent_block_hash: self.create_hash(),
            proposer_index: self.create_u64(),
            payload_attributes: PayloadAttributes {
                timestamp: self.create_u64(),
                prev_randao: self.create_hash(),
                suggested_fee_recipient: self.create_address(),
                withdrawals: None,
                parent_beacon_block_root: Some(self.create_hash()),
            },
        }
    }

    pub fn create_u64(&mut self) -> u64 {
        self.last_used_id += 1;
        self.last_used_id
    }

    pub fn create_u256(&mut self) -> U256 {
        U256::from(self.create_u64())
    }

    pub fn create_u8(&mut self) -> u8 {
        self.create_u64() as u8
    }

    pub fn create_address(&mut self) -> Address {
        Address::repeat_byte(self.create_u8())
    }

    pub fn create_hash(&mut self) -> B256 {
        B256::from(self.create_u256())
    }

    pub fn create_key(&mut self) -> BlsPublicKey {
        BlsPublicKey::repeat_byte(self.create_u8())
    }

    pub fn create_bloom(&mut self) -> Bloom {
        Bloom::repeat_byte(self.create_u8())
    }

    pub fn create_signature(&mut self) -> BlsSignature {
        BlsSignature::from([self.create_u8(); 96])
    }

    pub fn create_commitment(&mut self) -> Bytes48 {
        Bytes48::from([self.create_u8(); 48])
    }

    pub fn create_proof(&mut self) -> Bytes48 {
        Bytes48::from([self.create_u8(); 48])
    }

    pub fn create_blob(&mut self) -> Blob {
        Blob::from([self.create_u8(); 131_072])
    }
}
