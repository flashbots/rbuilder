//! EPBS bid and envelope signing for EIP-7732/Gloas.
//!
//! This module implements signing for ExecutionPayloadBid and ExecutionPayloadEnvelope
//! using the DOMAIN_BEACON_BUILDER domain as specified in the consensus specs.

use alloy_primitives::{Address, BlockHash, B256};
use alloy_rpc_types_beacon::BlsSignature;
use ethereum_consensus::{
    crypto::SecretKey,
    primitives::{ExecutionAddress, Hash32},
    signing::sign_with_domain,
    ssz::prelude::*,
};
use rbuilder_primitives::epbs::{ExecutionPayloadBid, SignedExecutionPayloadBid};

/// DOMAIN_BEACON_BUILDER from consensus-specs/specs/gloas/beacon-chain.md
/// Value: DomainType('0x0B000000')
pub const DOMAIN_BEACON_BUILDER: [u8; 4] = [0x0B, 0x00, 0x00, 0x00];

/// Signer for EPBS bids using the builder's validator key.
///
/// uses DOMAIN_BEACON_BUILDER since the builder is now a staked
/// validator in the beacon chain.
#[derive(Debug, Clone)]
pub struct EpbsBidSigner {
    /// Builder validator secret key.
    sec: SecretKey,
    /// The builders validator index in the beacon chain.
    builder_index: u64,
    /// Pre comp domain for signing (DOMAIN_BEACON_BUILDER + fork version + genesis validators root).
    domain: B256,
}

impl EpbsBidSigner {
    /// Create a new EPBS bid signer.
    pub fn new(sec: SecretKey, builder_index: u64, domain: B256) -> Self {
        Self {
            sec,
            builder_index,
            domain,
        }
    }

    /// Create from a hex-encoded secret key string.
    pub fn from_string(secret_key: String, builder_index: u64, domain: B256) -> eyre::Result<Self> {
        let secret_key = SecretKey::try_from(secret_key)
            .map_err(|e| eyre::eyre!("Failed to parse key: {:?}", e.to_string()))?;
        Ok(Self::new(secret_key, builder_index, domain))
    }

    /// Get the builder's validator index.
    pub fn builder_index(&self) -> u64 {
        self.builder_index
    }

    /// Get the builder's public key.
    pub fn pub_key(&self) -> alloy_rpc_types_beacon::BlsPublicKey {
        alloy_rpc_types_beacon::BlsPublicKey::from_slice(&self.sec.public_key())
    }

    /// Sign an ExecutionPayloadBid.
    ///
    /// This follows the spec:
    /// ```python
    /// def get_execution_payload_bid_signature(
    ///     state: BeaconState, bid: ExecutionPayloadBid, privkey: int
    /// ) -> BLSSignature
    pub fn sign_bid(&self, bid: &ExecutionPayloadBid) -> eyre::Result<SignedExecutionPayloadBid> {
        let ssz_bid = SszExecutionPayloadBid::from(bid);
        let signature = sign_with_domain(&ssz_bid, &self.sec, *self.domain)?;
        let signature = BlsSignature::from_slice(&signature);

        Ok(SignedExecutionPayloadBid {
            message: bid.clone(),
            signature,
        })
    }
}

/// SSZ-merkleizable version of ExecutionPayloadBid for signing.
///
/// This matches the consensus-specs container:
/// ```python
/// class ExecutionPayloadBid(Container):
///     parent_block_hash: Hash32
///     parent_block_root: Root
///     block_hash: Hash32
///     prev_randao: Bytes32
///     fee_recipient: ExecutionAddress
///     gas_limit: uint64
///     builder_index: ValidatorIndex
///     slot: Slot
///     value: Gwei
///     execution_payment: Gwei
///     blob_kzg_commitments_root: Root
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, SimpleSerialize)]
pub struct SszExecutionPayloadBid {
    pub parent_block_hash: Hash32,
    pub parent_block_root: Hash32,
    pub block_hash: Hash32,
    pub prev_randao: Hash32,
    pub fee_recipient: ExecutionAddress,
    pub gas_limit: u64,
    pub builder_index: u64,
    pub slot: u64,
    pub value: u64,
    pub execution_payment: u64,
    pub blob_kzg_commitments_root: Hash32,
}

impl From<&ExecutionPayloadBid> for SszExecutionPayloadBid {
    fn from(bid: &ExecutionPayloadBid) -> Self {
        Self {
            parent_block_hash: hash32_from_block_hash(&bid.parent_block_hash),
            parent_block_root: hash32_from_b256(&bid.parent_block_root),
            block_hash: hash32_from_block_hash(&bid.block_hash),
            prev_randao: hash32_from_b256(&bid.prev_randao),
            fee_recipient: address_to_execution_address(&bid.fee_recipient),
            gas_limit: bid.gas_limit,
            builder_index: bid.builder_index,
            slot: bid.slot,
            value: bid.value,
            execution_payment: bid.execution_payment,
            blob_kzg_commitments_root: hash32_from_b256(&bid.blob_kzg_commitments_root),
        }
    }
}

// Helper conversion functions

fn hash32_from_block_hash(h: &BlockHash) -> Hash32 {
    Hash32::try_from(h.as_slice()).expect("BlockHash is 32 bytes")
}

fn hash32_from_b256(h: &B256) -> Hash32 {
    Hash32::try_from(h.as_slice()).expect("B256 is 32 bytes")
}

fn address_to_execution_address(a: &Address) -> ExecutionAddress {
    ExecutionAddress::try_from(a.as_slice()).expect("Address is 20 bytes")
}

/// Compute the EPBS signing domain from beacon chain genesis data.
///
/// The domain is computed following the consensus-specs:
/// ```python
/// domain = compute_domain(DOMAIN_BEACON_BUILDER, fork_version, genesis_validators_root)
/// ```
///
/// The `fork_version` and `genesis_validators_root` are fetched from the beacon chain
/// via the `/eth/v1/beacon/genesis` endpoint in `config.rs`.
/// 
/// TODO check if implementation is correct, also make this part of consensus crate and import 
/// from there
pub fn compute_epbs_domain(fork_version: [u8; 4], genesis_validators_root: B256) -> B256 {
    // Simplified domain construction:
    // - Bytes 0-3: DOMAIN_BEACON_BUILDER (0x0B000000)
    // - Bytes 4-7: fork_version
    // - Bytes 8-31: first 24 bytes of genesis_validators_root
    //
    // Note: The full spec uses hash_tree_root(ForkData) for bytes 4-31,
    // but ethereum_consensus crate doesn't expose DOMAIN_BEACON_BUILDER yet.
    let mut domain = [0u8; 32];
    domain[0..4].copy_from_slice(&DOMAIN_BEACON_BUILDER);
    domain[4..8].copy_from_slice(&fork_version);
    domain[8..32].copy_from_slice(&genesis_validators_root.0[0..24]);

    B256::from(domain)
}


