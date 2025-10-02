use crate::utils::u256decimal_serde_helper;
use alloy_primitives::{Address, BlockHash, FixedBytes, B256, U256};
use alloy_rpc_types_beacon::{
    events::PayloadAttributesData, relay::BidTrace, BlsPublicKey, BlsSignature,
};
use ethereum_consensus::{
    crypto::SecretKey,
    primitives::{BlsPublicKey as BlsPublicKey2, ExecutionAddress, Hash32},
    signing::sign_with_domain,
    ssz::prelude::*,
};
use reth_primitives::SealedBlock;
use serde_with::{serde_as, DisplayFromStr};

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

    pub fn from_string(secret_key: String, domain: B256) -> eyre::Result<Self> {
        let secret_key = SecretKey::try_from(secret_key)
            .map_err(|e| eyre::eyre!("Failed to parse key: {:?}", e.to_string()))?;
        Self::new(secret_key, domain)
    }

    pub fn sign_payload(&self, bid_trace: &BidTrace) -> eyre::Result<Vec<u8>> {
        // We use RPCBidTrace not because of it's RPC nature but because it's also Merkleized
        let bid_trace = marshal_bid_trace(bid_trace);
        let signature = sign_with_domain(&bid_trace, &self.sec, *self.domain)?;
        Ok(signature.to_vec())
    }

    pub fn pub_key(&self) -> BlsPublicKey {
        BlsPublicKey::from_slice(&self.sec.public_key())
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
    // Should not panic since both types are equal in size
    BlsPublicKey2::try_from(k.as_slice()).unwrap()
}

fn a2e_address(a: &Address) -> ExecutionAddress {
    // Should not panic since Address matches ExecutionAddress size
    ExecutionAddress::try_from(a.as_slice()).unwrap()
}

pub fn sign_block_for_relay(
    signer: &BLSBlockSigner,
    sealed_block: &SealedBlock,
    attrs: &PayloadAttributesData,
    proposer_pubkey: BlsPublicKey,
    value: U256,
) -> eyre::Result<(BidTrace, BlsSignature)> {
    let message = BidTrace {
        slot: attrs.proposal_slot,
        parent_hash: attrs.parent_block_hash,
        block_hash: sealed_block.hash(),
        builder_pubkey: signer.pub_key(),
        proposer_pubkey,
        proposer_fee_recipient: attrs.payload_attributes.suggested_fee_recipient,
        gas_limit: sealed_block.gas_limit,
        gas_used: sealed_block.gas_used,
        value,
    };
    let signature = signer.sign_payload(&message)?;
    Ok((message, FixedBytes::from_slice(&signature)))
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
        let expected_key = BlsPublicKey::from_slice(&alloy_primitives::bytes!("a1885d66bef164889a2e35845c3b626545d7b0e513efe335e97c3a45e534013fa3bc38c3b7e6143695aecc4872ac52c4").0);
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
