use super::{
    CapellaSubmitBlockRequest, DenebSubmitBlockRequest, ElectraSubmitBlockRequest,
    SubmitBlockRequest,
};
use crate::utils::u256decimal_serde_helper;
use alloy_primitives::{Address, BlockHash, FixedBytes, B256, U256};
use alloy_rpc_types_beacon::{
    relay::{BidTrace, SignedBidSubmissionV2, SignedBidSubmissionV3, SignedBidSubmissionV4},
    BlsPublicKey,
};
use alloy_rpc_types_engine::{
    BlobsBundleV1, ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3, ExecutionPayloadV4,
};
use alloy_rpc_types_eth::Withdrawal;
use ethereum_consensus::{
    crypto::SecretKey,
    primitives::{BlsPublicKey as BlsPublicKey2, ExecutionAddress, Hash32},
    signing::sign_with_domain,
    ssz::prelude::*,
};
use primitive_types::H384;
use reth::rpc::types::beacon::events::PayloadAttributesData;
use reth_chainspec::{ChainSpec, EthereumHardforks};
use reth_primitives::{BlobTransactionSidecar, SealedBlock};
use serde_with::{serde_as, DisplayFromStr};
use std::sync::Arc;

/// Object to sign blocks to be sent to relays.
#[derive(Debug, Clone)]
pub struct BLSBlockSigner {
    sec: SecretKey,
    domain: B256,
}

impl BLSBlockSigner {
    pub fn new(sec: SecretKey, domain: B256) -> eyre::Result<Self> {
        Ok(Self { sec, domain })
    }

    pub fn sign_payload(&self, bid_trace: &BidTrace) -> eyre::Result<Vec<u8>> {
        // We use RPCBidTrace not because of it's RPC nature but because it's also Merkleized
        let bid_trace = marshal_bid_trace(bid_trace);

        let signature = sign_with_domain(&bid_trace, &self.sec, *self.domain)?;
        Ok(signature.to_vec())
    }

    pub fn pub_key(&self) -> H384 {
        H384::from_slice(self.sec.public_key().as_slice())
    }

    pub fn test_signer() -> Self {
        let key = alloy_primitives::fixed_bytes!(
            "5eae315483f028b5cdd5d1090ff0c7618b18737ea9bf3c35047189db22835c48"
        );
        let sec = SecretKey::try_from(key.as_slice()).unwrap();
        Self::new(sec, Default::default()).expect("failed to contruct signer")
    }
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
    builder_pubkey: BlsPublicKey2,
    proposer_pubkey: BlsPublicKey2,
    proposer_fee_recipient: ExecutionAddress,
    #[serde_as(as = "DisplayFromStr")]
    gas_limit: u64,
    #[serde_as(as = "DisplayFromStr")]
    gas_used: u64,
    #[serde(with = "u256decimal_serde_helper")]
    value: U256,
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

fn a2e_hash32(h: &BlockHash) -> Hash32 {
    // Should not panic since BlockHash matches Hash32 size
    Hash32::try_from(h.as_slice()).unwrap()
}

fn a2e_pubkey(k: &BlsPublicKey) -> BlsPublicKey2 {
    // Should not panic since H384 matches BlsPublicKey size
    BlsPublicKey2::try_from(k.as_slice()).unwrap()
}

fn a2e_address(a: &Address) -> ExecutionAddress {
    // Should not panic since Address matches ExecutionAddress size
    ExecutionAddress::try_from(a.as_slice()).unwrap()
}

pub fn sign_block_for_relay(
    signer: &BLSBlockSigner,
    sealed_block: &SealedBlock,
    blobs_bundle: &[Arc<BlobTransactionSidecar>],
    chain_spec: &ChainSpec,
    attrs: &PayloadAttributesData,
    pubkey: H384,
    value: U256,
) -> eyre::Result<SubmitBlockRequest> {
    let message = BidTrace {
        slot: attrs.proposal_slot,
        parent_hash: attrs.parent_block_hash,
        block_hash: sealed_block.hash(),
        builder_pubkey: FixedBytes::from_slice(signer.pub_key().as_bytes()),
        proposer_pubkey: FixedBytes::from_slice(pubkey.as_bytes()),
        proposer_fee_recipient: attrs.payload_attributes.suggested_fee_recipient,
        gas_limit: sealed_block.gas_limit,
        gas_used: sealed_block.gas_used,
        value,
    };

    let signature = signer.sign_payload(&message)?;
    let signature = FixedBytes::from_slice(&signature);

    let capella_payload = ExecutionPayloadV2 {
        payload_inner: ExecutionPayloadV1 {
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
            transactions: sealed_block
                .body
                .iter()
                .map(|tx| tx.envelope_encoded().to_vec().into())
                .collect(),
        },
        withdrawals: sealed_block
            .withdrawals
            .clone()
            .map(|w| {
                w.into_iter()
                    .map(|w| Withdrawal {
                        index: w.index,
                        validator_index: w.validator_index,
                        address: w.address,
                        amount: w.amount,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
    };

    let submit_block_request = if chain_spec.is_cancun_active_at_timestamp(sealed_block.timestamp) {
        let execution_payload = ExecutionPayloadV3 {
            payload_inner: capella_payload,
            blob_gas_used: sealed_block
                .blob_gas_used
                .expect("deneb block does not have blob gas used"),
            excess_blob_gas: sealed_block
                .excess_blob_gas
                .expect("deneb block does not have excess blob gas"),
        };

        let blobs_bundle = marshal_txs_blobs_sidecars(blobs_bundle);

        if chain_spec.is_prague_active_at_timestamp(sealed_block.timestamp) {
            let mut deposit_requests = Vec::new();
            let mut withdrawal_requests = Vec::new();
            let mut consolidation_requests = Vec::new();
            for request in sealed_block.requests.iter().flat_map(|r| &r.0) {
                match request {
                    alloy_consensus::Request::DepositRequest(r) => {
                        deposit_requests.push(*r);
                    }
                    alloy_consensus::Request::WithdrawalRequest(r) => {
                        withdrawal_requests.push(*r);
                    }
                    alloy_consensus::Request::ConsolidationRequest(r) => {
                        consolidation_requests.push(*r);
                    }
                    _ => {}
                };
            }

            let execution_payload = ExecutionPayloadV4 {
                payload_inner: execution_payload,
                deposit_requests,
                withdrawal_requests,
                consolidation_requests,
            };
            SubmitBlockRequest::Electra(ElectraSubmitBlockRequest(SignedBidSubmissionV4 {
                message,
                execution_payload,
                blobs_bundle,
                signature,
            }))
        } else {
            SubmitBlockRequest::Deneb(DenebSubmitBlockRequest(SignedBidSubmissionV3 {
                message,
                execution_payload,
                blobs_bundle,
                signature,
            }))
        }
    } else {
        let execution_payload = capella_payload;
        SubmitBlockRequest::Capella(CapellaSubmitBlockRequest(SignedBidSubmissionV2 {
            message,
            execution_payload,
            signature,
        }))
    };

    Ok(submit_block_request)
}

fn flatten_marshal<Source>(
    txs_blobs_sidecars: &[Arc<BlobTransactionSidecar>],
    vec_getter: impl Fn(&Arc<BlobTransactionSidecar>) -> Vec<Source>,
) -> Vec<Source> {
    let flatten_data = txs_blobs_sidecars.iter().flat_map(vec_getter);
    flatten_data.collect::<Vec<Source>>()
}

fn marshal_txs_blobs_sidecars(txs_blobs_sidecars: &[Arc<BlobTransactionSidecar>]) -> BlobsBundleV1 {
    let rpc_commitments = flatten_marshal(txs_blobs_sidecars, |t| t.commitments.clone());
    let rpc_proofs = flatten_marshal(txs_blobs_sidecars, |t| t.proofs.clone());
    let rpc_blobs = flatten_marshal(txs_blobs_sidecars, |t| t.blobs.clone());

    BlobsBundleV1 {
        commitments: rpc_commitments,
        proofs: rpc_proofs,
        blobs: rpc_blobs,
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_private_pub_key() {
        let key = alloy_primitives::fixed_bytes!(
            "5eae315483f028b5cdd5d1090ff0c7618b18737ea9bf3c35047189db22835c48"
        );
        let sec = SecretKey::try_from(key.as_slice()).unwrap();
        let signer =
            super::BLSBlockSigner::new(sec, Default::default()).expect("failed to contruct signer");

        let pub_key = signer.pub_key();
        let expected_key = H384::from_slice(&alloy_primitives::bytes!("a1885d66bef164889a2e35845c3b626545d7b0e513efe335e97c3a45e534013fa3bc38c3b7e6143695aecc4872ac52c4").0);
        assert_eq!(pub_key, expected_key);
    }

    #[test]
    fn test_sign_bid() {
        let key = alloy_primitives::fixed_bytes!(
            "5eae315483f028b5cdd5d1090ff0c7618b18737ea9bf3c35047189db22835c48"
        );

        let sec = SecretKey::try_from(key.as_slice()).unwrap();
        let signer =
            super::BLSBlockSigner::new(sec, Default::default()).expect("failed to contruct signer");

        let s = r#"{"slot":"1","parent_hash":"0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2","block_hash":"0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2","builder_pubkey":"0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a", "proposer_pubkey": "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a","proposer_fee_recipient":"0xabcf8e0d4e9587369b2301d0790347320302cc09","gas_limit":"1","gas_used":"1","value":"1"}"#;
        let bid = serde_json::from_str::<BidTrace>(s).unwrap();

        let signature = signer.sign_payload(&bid).unwrap();

        let expected = alloy_primitives::hex::decode("97b98dd2323c89e4dbf0f9e7c8da092df0b4e3bf684a3da53ddc2eb4381b8a074a4e6fbf806166490cd8ca142ce298720fe08bba84d2e42dc09d76c46a26ca5595eeed35fed1d16c4bd2ece99138384500b1d8994ec11f64c9b89f60041c70dc").unwrap();
        assert_eq!(signature, expected);
    }
}
