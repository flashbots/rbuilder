//! This file contains conversion funcs to ethereum_consensus lib so we can user their classes for relay communication since they are already json/ssz ready.

use std::sync::Arc;

use super::{
    BidTrace, CapellaExecutionPayload, CapellaSubmitBlockRequest, DenebExecutionPayload,
    DenebSubmitBlockRequest, SubmitBlockRequest,
};
use crate::utils::u256decimal_serde_helper;
use alloy_primitives::{Address, BlockHash, Bloom, Bytes, U256};
use ethereum_consensus::{
    deneb::{
        mainnet::{BYTES_PER_BLOB, MAX_BLOB_COMMITMENTS_PER_BLOCK},
        polynomial_commitments::{KzgCommitment, KzgProof, BYTES_PER_COMMITMENT, BYTES_PER_PROOF},
    },
    primitives::{BlsPublicKey, ExecutionAddress, Hash32},
    ssz::prelude::*,
};

use itertools::Itertools;
use primitive_types::H384;
use reth::primitives::{
    kzg::{Blob, Bytes48},
    BlobTransactionSidecar,
};
use serde_with::{serde_as, DisplayFromStr};

type RPCCapellaExecutionPayload = ethereum_consensus::capella::presets::mainnet::ExecutionPayload;
type RPCDenebExecutionPayload = ethereum_consensus::deneb::presets::mainnet::ExecutionPayload;

// Acording to https://github.com/ethereum/builder-specs/blob/18c435e360192aa39c378584fe14a3158f30dfbf/specs/deneb/builder.md?plain=1#L35-L38
// and https://github.com/ethereum/builder-specs/blob/18c435e360192aa39c378584fe14a3158f30dfbf/types/deneb/blobs_bundle.yaml#L17
// the proofs, blobs and commitments share the same max MAX_BLOB_COMMITMENTS_PER_BLOCK = 4096
type RPCBlob = ByteVector<BYTES_PER_BLOB>;
type RPCCommitmentList = List<KzgCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK>;
type RPCProofList = List<KzgProof, MAX_BLOB_COMMITMENTS_PER_BLOCK>;
type RPCBlobList = List<RPCBlob, MAX_BLOB_COMMITMENTS_PER_BLOCK>;

// couldn't find it somewhere else :(
const SIGNATURE_SIZE: usize = 96;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Too many txs")]
    TooManyTxs,
    #[error("Tx to big")]
    TxTooBig,
    #[error("Extra data too big")]
    ExtraDataTooBig,
    #[error("Too many withdrawals")]
    TooManyWithdrawals,
    #[error("Invalid signature")]
    InvalidSignature,
    #[error("Wrong KzgCommitment size")]
    WrongKzgCommitmentSize,
    #[error("Too many KzgCommitment")]
    TooManyKzgCommitments,
    #[error("Too many blobs")]
    TooManyBlobs,
    #[error("Blob to big")]
    BlobTooBig,
    #[error("Wrong proof size")]
    WrongProofSize,
    #[error("Too many proofs")]
    TooManyProofs,
}

#[serde_as]
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, SimpleSerialize, serde::Serialize, serde::Deserialize,
)]

pub struct RPCBidTrace {
    #[serde_as(as = "DisplayFromStr")]
    slot: u64,
    parent_hash: Hash32,
    block_hash: Hash32,
    builder_pubkey: BlsPublicKey,
    proposer_pubkey: BlsPublicKey,
    proposer_fee_recipient: ExecutionAddress,
    #[serde_as(as = "DisplayFromStr")]
    gas_limit: u64,
    #[serde_as(as = "DisplayFromStr")]
    gas_used: u64,
    #[serde(with = "u256decimal_serde_helper")]
    value: U256,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, SimpleSerialize)]
#[serde(untagged)]
#[ssz(transparent)]
pub enum RPCSubmitBlockRequest {
    Capella(RPCCapellaSubmitBlockRequest),
    Deneb(RPCDenebSubmitBlockRequest),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, SimpleSerialize)]
pub struct RPCCapellaSubmitBlockRequest {
    pub message: RPCBidTrace,
    pub execution_payload: RPCCapellaExecutionPayload,
    pub signature: ethereum_consensus::primitives::BlsSignature,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, SimpleSerialize)]
pub struct RPCBlobsBundle {
    pub commitments: RPCCommitmentList,
    pub proofs: RPCProofList,
    pub blobs: RPCBlobList,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, SimpleSerialize)]
pub struct RPCDenebSubmitBlockRequest {
    pub message: RPCBidTrace,
    pub execution_payload: RPCDenebExecutionPayload,
    pub blobs_bundle: RPCBlobsBundle,
    pub signature: ethereum_consensus::primitives::BlsSignature,
}

pub fn marshal_submit_block_request(
    req: &SubmitBlockRequest,
) -> Result<RPCSubmitBlockRequest, Error> {
    match req {
        SubmitBlockRequest::Capella(c) => Ok(RPCSubmitBlockRequest::Capella(
            marshal_capella_submit_block_request(c)?,
        )),
        SubmitBlockRequest::Deneb(d) => Ok(RPCSubmitBlockRequest::Deneb(
            marshal_deneb_submit_block_request(d)?,
        )),
    }
}

pub fn marshal_capella_submit_block_request(
    req: &CapellaSubmitBlockRequest,
) -> Result<RPCCapellaSubmitBlockRequest, Error> {
    Ok(RPCCapellaSubmitBlockRequest {
        message: marshal_bid_trace(&req.message),
        execution_payload: marshal_capella_payload(&req.execution_payload)?,
        signature: a2e_signature(&req.signature)?,
    })
}

pub fn marshal_deneb_submit_block_request(
    req: &DenebSubmitBlockRequest,
) -> Result<RPCDenebSubmitBlockRequest, Error> {
    Ok(RPCDenebSubmitBlockRequest {
        message: marshal_bid_trace(&req.message),
        execution_payload: marshal_deneb_payload(&req.execution_payload)?,
        blobs_bundle: marshal_txs_blobs_sidecars(&req.txs_blobs_sidecars)?,
        signature: a2e_signature(&req.signature)?,
    })
}

///For all txts sidecars takes one of the vectors via vec_getter and transforms all the elements via data_converter.
fn flatten_marshal<Source, Dest>(
    txs_blobs_sidecars: &[Arc<BlobTransactionSidecar>],
    vec_getter: impl Fn(&Arc<BlobTransactionSidecar>) -> &Vec<Source>,
    data_converter: impl Fn(&Source) -> Result<Dest, Error>,
) -> Result<Vec<Dest>, Error> {
    let flatten_data = txs_blobs_sidecars.iter().flat_map(vec_getter);
    flatten_data
        .map(data_converter)
        .collect::<Result<Vec<Dest>, Error>>()
}

fn marshal_txs_blobs_sidecars(
    txs_blobs_sidecars: &[Arc<BlobTransactionSidecar>],
) -> Result<RPCBlobsBundle, Error> {
    let rpc_commitments = flatten_marshal(
        txs_blobs_sidecars,
        |t| &t.commitments,
        |c| KzgCommitment::try_from(c.as_ref()).map_err(|_| Error::WrongKzgCommitmentSize),
    )?;
    let rpc_proofs = flatten_marshal(
        txs_blobs_sidecars,
        |t| &t.proofs,
        |p| KzgProof::try_from(p.as_ref()).map_err(|_| Error::WrongProofSize),
    )?;

    let rpc_blobs = flatten_marshal(
        txs_blobs_sidecars,
        |t| &t.blobs,
        |blob| RPCBlob::try_from(blob.as_ref()).map_err(|_| Error::BlobTooBig),
    )?;

    Ok(RPCBlobsBundle {
        commitments: RPCCommitmentList::try_from(rpc_commitments)
            .map_err(|_| Error::TooManyKzgCommitments)?,
        proofs: RPCProofList::try_from(rpc_proofs).map_err(|_| Error::TooManyProofs)?,
        blobs: RPCBlobList::try_from(rpc_blobs).map_err(|_| Error::TooManyBlobs)?,
    })
}

pub fn marshal_deneb_payload(
    payload: &DenebExecutionPayload,
) -> Result<RPCDenebExecutionPayload, Error> {
    Ok(RPCDenebExecutionPayload {
        parent_hash: a2e_hash32(&payload.capella_payload.parent_hash),
        fee_recipient: a2e_address(&payload.capella_payload.fee_recipient),
        state_root: a2e_hash32(&payload.capella_payload.state_root),
        receipts_root: a2e_hash32(&payload.capella_payload.receipts_root),
        logs_bloom: a2e_bloom(&payload.capella_payload.logs_bloom),
        prev_randao: a2e_hash32(&payload.capella_payload.prev_randao),
        block_number: payload.capella_payload.block_number,
        gas_limit: payload.capella_payload.gas_limit,
        gas_used: payload.capella_payload.gas_used,
        timestamp: payload.capella_payload.timestamp,
        extra_data: ByteList::try_from(payload.capella_payload.extra_data.as_ref())
            .map_err(|_| Error::ExtraDataTooBig)?,
        base_fee_per_gas: payload.capella_payload.base_fee_per_gas,
        block_hash: a2e_hash32(&payload.capella_payload.block_hash),
        transactions: marshal_txs(&payload.capella_payload.transactions)?,
        withdrawals: marshal_withdrawals(&payload.capella_payload.withdrawals)?,
        blob_gas_used: payload.blob_gas_used,
        excess_blob_gas: payload.excess_blob_gas,
    })
}

pub fn marshal_capella_payload(
    payload: &CapellaExecutionPayload,
) -> Result<RPCCapellaExecutionPayload, Error> {
    Ok(RPCCapellaExecutionPayload {
        parent_hash: a2e_hash32(&payload.parent_hash),
        fee_recipient: a2e_address(&payload.fee_recipient),
        state_root: a2e_hash32(&payload.state_root),
        receipts_root: a2e_hash32(&payload.receipts_root),
        logs_bloom: a2e_bloom(&payload.logs_bloom),
        prev_randao: a2e_hash32(&payload.prev_randao),
        block_number: payload.block_number,
        gas_limit: payload.gas_limit,
        gas_used: payload.gas_used,
        timestamp: payload.timestamp,
        extra_data: ByteList::try_from(payload.extra_data.as_ref())
            .map_err(|_| Error::ExtraDataTooBig)?,
        base_fee_per_gas: payload.base_fee_per_gas,
        block_hash: a2e_hash32(&payload.block_hash),
        transactions: marshal_txs(&payload.transactions)?,
        withdrawals: marshal_withdrawals(&payload.withdrawals)?,
    })
}

fn marshal_txs<
    const MAX_BYTES_PER_TRANSACTION: usize,
    const MAX_TRANSACTIONS_PER_PAYLOAD: usize,
>(
    txs: &[Bytes],
) -> Result<List<ByteList<MAX_BYTES_PER_TRANSACTION>, MAX_TRANSACTIONS_PER_PAYLOAD>, Error> {
    let txs = txs
        .iter()
        .map(|tx| {
            ByteList::<MAX_BYTES_PER_TRANSACTION>::try_from(tx.as_ref())
                .map_err(|_| Error::TxTooBig)
        })
        .collect::<Result<Vec<_>, _>>()?;
    List::try_from(txs).map_err(|_| Error::TooManyTxs)
}

fn marshal_withdrawals<const MAX_WITHDRAWALS_PER_PAYLOAD: usize>(
    wds: &[super::Withdrawal],
) -> Result<List<ethereum_consensus::capella::Withdrawal, MAX_WITHDRAWALS_PER_PAYLOAD>, Error> {
    let wds = wds.iter().map(a2e_withdrawal).collect_vec();
    List::try_from(wds).map_err(|_| Error::TooManyWithdrawals)
}

pub fn marshal_bid_trace(bid_trace: &BidTrace) -> RPCBidTrace {
    RPCBidTrace {
        slot: bid_trace.slot,
        parent_hash: a2e_hash32(&bid_trace.parent_hash),
        block_hash: a2e_hash32(&bid_trace.block_hash),
        builder_pubkey: a2e_pubkey(&bid_trace.builder_pubkey),
        proposer_pubkey: a2e_pubkey(&bid_trace.proposer_pubkey),
        proposer_fee_recipient: a2e_address(&bid_trace.proposer_fee_recipient),
        gas_limit: bid_trace.gas_limit,
        gas_used: bid_trace.gas_used,
        value: bid_trace.value,
    }
}

/// TestDataGenerator allows you to create unique test objects with unique content, it tries to use different numbers for every field it sets
#[derive(Default)]
pub struct TestDataGenerator {
    last_used_id: u64,
}

impl TestDataGenerator {
    pub fn create_deneb_submit_block_request(&mut self) -> DenebSubmitBlockRequest {
        DenebSubmitBlockRequest {
            message: self.create_bid_trace(),
            execution_payload: self.create_deneb_payload(),
            txs_blobs_sidecars: self.create_txs_blobs_sidecars(),
            signature: self.create_signature(),
        }
    }

    pub fn create_txs_blobs_sidecars(&mut self) -> Vec<Arc<BlobTransactionSidecar>> {
        vec![Arc::new(BlobTransactionSidecar {
            commitments: vec![self.create_commitment()],
            proofs: vec![self.create_proof()],
            blobs: vec![self.create_blob()],
        })]
    }

    pub fn create_deneb_payload(&mut self) -> DenebExecutionPayload {
        DenebExecutionPayload {
            capella_payload: self.create_capella_payload(),
            blob_gas_used: self.create_u64(),
            excess_blob_gas: self.create_u64(),
        }
    }

    pub fn create_capella_submit_block_request(&mut self) -> CapellaSubmitBlockRequest {
        CapellaSubmitBlockRequest {
            message: self.create_bid_trace(),
            execution_payload: self.create_capella_payload(),
            signature: self.create_signature(),
        }
    }

    pub fn create_withdrawal(&mut self) -> super::Withdrawal {
        super::Withdrawal {
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

    pub fn create_capella_payload(&mut self) -> CapellaExecutionPayload {
        CapellaExecutionPayload {
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
            withdrawals: vec![self.create_withdrawal()],
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

    pub fn create_hash(&mut self) -> BlockHash {
        BlockHash::from(self.create_u256())
    }

    pub fn create_key(&mut self) -> H384 {
        H384::repeat_byte(self.create_u8())
    }

    pub fn create_bloom(&mut self) -> Bloom {
        Bloom::repeat_byte(self.create_u8())
    }

    pub fn create_signature(&mut self) -> Bytes {
        Bytes::from([self.create_u8(); SIGNATURE_SIZE])
    }

    pub fn create_commitment(&mut self) -> Bytes48 {
        Bytes48::from([self.create_u8(); BYTES_PER_COMMITMENT])
    }

    pub fn create_proof(&mut self) -> Bytes48 {
        Bytes48::from([self.create_u8(); BYTES_PER_PROOF])
    }

    pub fn create_blob(&mut self) -> Blob {
        Blob::from([self.create_u8(); BYTES_PER_BLOB])
    }
}

// Functions from types using alloy primitives to ethereum_consensus types

fn a2e_hash32(h: &BlockHash) -> Hash32 {
    // Should not panic since BlockHash matches Hash32 size
    Hash32::try_from(h.as_slice()).unwrap()
}

fn a2e_bloom(bloom: &Bloom) -> ByteVector<256> {
    // Should not panic since Bloom matches ByteVector<256> size
    ByteVector::try_from(bloom.as_slice()).unwrap()
}

fn a2e_pubkey(k: &H384) -> BlsPublicKey {
    // Should not panic since H384 matches BlsPublicKey size
    BlsPublicKey::try_from(k.as_bytes()).unwrap()
}

fn a2e_address(a: &Address) -> ExecutionAddress {
    // Should not panic since Address matches ExecutionAddress size
    ExecutionAddress::try_from(a.as_slice()).unwrap()
}

fn a2e_signature(s: &Bytes) -> Result<ethereum_consensus::primitives::BlsSignature, Error> {
    ethereum_consensus::primitives::BlsSignature::try_from(s.as_ref())
        .map_err(|_| Error::InvalidSignature)
}

fn a2e_withdrawal(w: &super::Withdrawal) -> ethereum_consensus::capella::Withdrawal {
    ethereum_consensus::deneb::Withdrawal {
        index: w.index as usize,
        validator_index: w.validator_index as usize,
        address: a2e_address(&w.address),
        amount: w.amount,
    }
}
