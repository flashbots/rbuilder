use std::sync::Arc;

use super::{
    marshal_bid_trace, BidTrace, CapellaExecutionPayload, CapellaSubmitBlockRequest,
    DenebExecutionPayload, DenebSubmitBlockRequest, SubmitBlockRequest, Withdrawal,
};
use alloy_primitives::{B256, U256};
use ethereum_consensus::{crypto::SecretKey, signing::sign_with_domain};
use primitive_types::H384;
use reth::{
    primitives::{BlobTransactionSidecar, ChainSpec, SealedBlock},
    rpc::types::beacon::events::PayloadAttributesData,
};

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
        let mut bid_trace = marshal_bid_trace(bid_trace);
        let signature = sign_with_domain(&mut bid_trace, &self.sec, *self.domain)?;
        Ok(signature.to_vec())
    }

    pub fn pub_key(&self) -> H384 {
        H384::from_slice(self.sec.public_key().as_slice())
    }
}

pub fn sign_block_for_relay(
    signer: &BLSBlockSigner,
    sealed_block: &SealedBlock,
    txs_blobs_sidecars: &[Arc<BlobTransactionSidecar>],
    chain_spec: &ChainSpec,
    attrs: &PayloadAttributesData,
    pubkey: H384,
    value: U256,
) -> eyre::Result<SubmitBlockRequest> {
    let message = BidTrace {
        slot: attrs.proposal_slot,
        parent_hash: attrs.parent_block_hash,
        block_hash: sealed_block.hash(),
        builder_pubkey: signer.pub_key(),
        proposer_pubkey: pubkey,
        proposer_fee_recipient: attrs.payload_attributes.suggested_fee_recipient,
        gas_limit: sealed_block.gas_limit,
        gas_used: sealed_block.gas_used,
        value,
    };

    let signature = signer.sign_payload(&message)?.into();

    let capella_payload = CapellaExecutionPayload {
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
        let execution_payload = DenebExecutionPayload {
            capella_payload,
            blob_gas_used: sealed_block
                .blob_gas_used
                .expect("deneb block does not have blob gas used"),
            excess_blob_gas: sealed_block
                .excess_blob_gas
                .expect("deneb block does not have excess blob gas"),
        };
        SubmitBlockRequest::Deneb(DenebSubmitBlockRequest {
            message,
            execution_payload,
            txs_blobs_sidecars: txs_blobs_sidecars.to_vec(),
            signature,
        })
    } else {
        let execution_payload = capella_payload;
        SubmitBlockRequest::Capella(CapellaSubmitBlockRequest {
            message,
            execution_payload,
            signature,
        })
    };

    Ok(submit_block_request)
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
