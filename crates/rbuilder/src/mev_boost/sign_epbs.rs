//! EPBS bid and envelope signing for EIP-7732/Gloas.
//!
//! This module implements signing for ExecutionPayloadBid and ExecutionPayloadEnvelope
//! using the DOMAIN_BEACON_BUILDER domain as specified in the consensus specs.
//!
//! uses lh consensus types for ssz hash_tree_root computation

use alloy_primitives::B256;
use alloy_rpc_types_beacon::BlsSignature;
use ethereum_consensus::crypto::SecretKey;
use lighthouse_bls::{PublicKeyBytes, SignatureBytes};
use lighthouse_ssz_types::VariableList;
use lighthouse_types::{
    ConsolidationRequest, DepositRequest, ExecutionBlockHash,
    ExecutionPayloadBid as LhExecutionPayloadBid,
    ExecutionPayloadEnvelope as LhExecutionPayloadEnvelope, ExecutionPayloadGloas,
    ExecutionRequests as LhExecutionRequests, Hash256, KzgCommitments, MainnetEthSpec, SignedRoot,
    Slot, Withdrawal as LhWithdrawal, WithdrawalRequest,
};
use rbuilder_primitives::epbs::{
    ExecutionPayloadBid, ExecutionPayloadEnvelope, SignedExecutionPayloadBid,
    SignedExecutionPayloadEnvelope,
};

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

    /// Sign an ExecutionPayloadBid using lh ssz types.
    pub fn sign_bid(&self, bid: &ExecutionPayloadBid) -> eyre::Result<SignedExecutionPayloadBid> {
        let lh_bid = to_lh_bid(bid);
        let signing_root = lh_bid.signing_root(self.domain);
        let signature = self.sec.sign(signing_root.as_ref());
        let signature = BlsSignature::from_slice(&signature);

        Ok(SignedExecutionPayloadBid {
            message: bid.clone(),
            signature,
        })
    }

    /// Sign an ExecutionPayloadEnvelope using Lighthouse's SSZ types.
    pub fn sign_envelope(
        &self,
        envelope: &ExecutionPayloadEnvelope,
    ) -> eyre::Result<SignedExecutionPayloadEnvelope> {
        let lh_envelope = to_lh_envelope(envelope)?;
        let signing_root = lh_envelope.signing_root(self.domain);
        let signature = self.sec.sign(signing_root.as_ref());
        let signature = BlsSignature::from_slice(&signature);

        Ok(SignedExecutionPayloadEnvelope {
            message: envelope.clone(),
            signature,
        })
    }
}

// ---------------------------------------------------------------------------
// bid conversion: alloy types -> lh types
// ---------------------------------------------------------------------------

fn to_lh_bid(bid: &ExecutionPayloadBid) -> LhExecutionPayloadBid {
    let commitments_refs: Vec<&[u8]> = bid
        .blob_kzg_commitments
        .iter()
        .map(|c| c.as_ref())
        .collect();
    let commitments_root =
        rbuilder_primitives::mev_boost::ssz_roots::calculate_blob_kzg_commitments_root_ssz(
            &commitments_refs,
        );

    LhExecutionPayloadBid {
        parent_block_hash: ExecutionBlockHash::from_root(Hash256::from(bid.parent_block_hash)),
        parent_block_root: Hash256::from(bid.parent_block_root),
        block_hash: ExecutionBlockHash::from_root(Hash256::from(bid.block_hash)),
        prev_randao: Hash256::from(bid.prev_randao),
        fee_recipient: bid.fee_recipient,
        gas_limit: bid.gas_limit,
        builder_index: bid.builder_index,
        slot: Slot::new(bid.slot),
        value: bid.value,
        execution_payment: bid.execution_payment,
        blob_kzg_commitments_root: Hash256::from(commitments_root),
    }
}

// ---------------------------------------------------------------------------
// envelope conversion: alloy types -> lh types
// ---------------------------------------------------------------------------

fn to_lh_envelope(
    envelope: &ExecutionPayloadEnvelope,
) -> eyre::Result<LhExecutionPayloadEnvelope<MainnetEthSpec>> {
    let payload = to_lh_execution_payload(envelope)?;
    let execution_requests = to_lh_execution_requests(&envelope.execution_requests)?;

    let commitments: Vec<lighthouse_types::KzgCommitment> = envelope
        .blob_kzg_commitments
        .iter()
        .map(|c| {
            let mut bytes = [0u8; 48];
            let len = c.len().min(48);
            bytes[..len].copy_from_slice(&c[..len]);
            lighthouse_types::KzgCommitment(bytes)
        })
        .collect();
    let blob_kzg_commitments = KzgCommitments::<MainnetEthSpec>::new(commitments)
        .map_err(|e| eyre::eyre!("Too many blob KZG commitments: {:?}", e))?;

    Ok(LhExecutionPayloadEnvelope {
        payload,
        execution_requests,
        builder_index: envelope.builder_index,
        beacon_block_root: Hash256::from(envelope.beacon_block_root),
        slot: Slot::new(envelope.slot),
        blob_kzg_commitments,
        state_root: Hash256::from(envelope.state_root),
    })
}

fn to_lh_execution_payload(
    envelope: &ExecutionPayloadEnvelope,
) -> eyre::Result<ExecutionPayloadGloas<MainnetEthSpec>> {
    let inner1 = &envelope.payload.payload_inner.payload_inner;
    let inner2 = &envelope.payload.payload_inner;
    let inner3 = &envelope.payload;

    // convert transactions
    let transactions: Vec<_> = inner1
        .transactions
        .iter()
        .map(|tx| {
            VariableList::new(tx.to_vec())
                .map_err(|e| eyre::eyre!("Transaction too large: {:?}", e))
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    let transactions = VariableList::new(transactions)
        .map_err(|e| eyre::eyre!("Too many transactions: {:?}", e))?;

    // convert withdrawals
    let withdrawals: Vec<LhWithdrawal> = inner2
        .withdrawals
        .iter()
        .map(|w| LhWithdrawal {
            index: w.index,
            validator_index: w.validator_index,
            address: w.address,
            amount: w.amount,
        })
        .collect();
    let withdrawals =
        VariableList::new(withdrawals).map_err(|e| eyre::eyre!("Too many withdrawals: {:?}", e))?;

    // convert extra_data
    let extra_data = VariableList::new(inner1.extra_data.to_vec())
        .map_err(|e| eyre::eyre!("Extra data too long: {:?}", e))?;

    // convert logs_bloom
    let logs_bloom = lighthouse_ssz_types::FixedVector::new(inner1.logs_bloom.to_vec())
        .map_err(|e| eyre::eyre!("Invalid logs_bloom: {:?}", e))?;

    Ok(ExecutionPayloadGloas {
        parent_hash: ExecutionBlockHash::from_root(Hash256::from(inner1.parent_hash)),
        fee_recipient: inner1.fee_recipient,
        state_root: Hash256::from(inner1.state_root),
        receipts_root: Hash256::from(inner1.receipts_root),
        logs_bloom,
        prev_randao: Hash256::from(inner1.prev_randao),
        block_number: inner1.block_number,
        gas_limit: inner1.gas_limit,
        gas_used: inner1.gas_used,
        timestamp: inner1.timestamp,
        extra_data,
        base_fee_per_gas: inner1.base_fee_per_gas,
        block_hash: ExecutionBlockHash::from_root(Hash256::from(inner1.block_hash)),
        transactions,
        withdrawals,
        blob_gas_used: inner3.blob_gas_used,
        excess_blob_gas: inner3.excess_blob_gas,
    })
}

fn to_lh_execution_requests(
    requests: &rbuilder_primitives::epbs::ExecutionRequests,
) -> eyre::Result<LhExecutionRequests<MainnetEthSpec>> {
    let deposits: Vec<DepositRequest> = requests
        .deposits
        .iter()
        .filter(|raw| raw.len() >= 192)
        .map(|raw| {
            Ok(DepositRequest {
                pubkey: PublicKeyBytes::deserialize(&raw[0..48])
                    .map_err(|e| eyre::eyre!("deposit pubkey: {:?}", e))?,
                withdrawal_credentials: Hash256::from_slice(&raw[48..80]),
                amount: u64::from_le_bytes(raw[80..88].try_into().unwrap()),
                signature: SignatureBytes::deserialize(&raw[88..184])
                    .map_err(|e| eyre::eyre!("deposit signature: {:?}", e))?,
                index: u64::from_le_bytes(raw[184..192].try_into().unwrap()),
            })
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    let deposits = VariableList::new(deposits)
        .map_err(|e| eyre::eyre!("Too many deposit requests: {:?}", e))?;

    let withdrawals: Vec<WithdrawalRequest> = requests
        .withdrawals
        .iter()
        .filter(|raw| raw.len() >= 76)
        .map(|raw| {
            Ok(WithdrawalRequest {
                source_address: alloy_primitives::Address::from_slice(&raw[0..20]),
                validator_pubkey: PublicKeyBytes::deserialize(&raw[20..68])
                    .map_err(|e| eyre::eyre!("withdrawal validator_pubkey: {:?}", e))?,
                amount: u64::from_le_bytes(raw[68..76].try_into().unwrap()),
            })
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    let withdrawals = VariableList::new(withdrawals)
        .map_err(|e| eyre::eyre!("Too many withdrawal requests: {:?}", e))?;

    let consolidations: Vec<ConsolidationRequest> = requests
        .consolidations
        .iter()
        .filter(|raw| raw.len() >= 116)
        .map(|raw| {
            Ok(ConsolidationRequest {
                source_address: alloy_primitives::Address::from_slice(&raw[0..20]),
                source_pubkey: PublicKeyBytes::deserialize(&raw[20..68])
                    .map_err(|e| eyre::eyre!("consolidation source_pubkey: {:?}", e))?,
                target_pubkey: PublicKeyBytes::deserialize(&raw[68..116])
                    .map_err(|e| eyre::eyre!("consolidation target_pubkey: {:?}", e))?,
            })
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    let consolidations = VariableList::new(consolidations)
        .map_err(|e| eyre::eyre!("Too many consolidation requests: {:?}", e))?;

    Ok(LhExecutionRequests {
        deposits,
        withdrawals,
        consolidations,
    })
}

// ---------------------------------------------------------------------------
// domain computation
// ---------------------------------------------------------------------------

/// Compute the EPBS signing domain from beacon chain genesis data.
///
/// The domain is computed following the consensus-specs:
/// ```python
/// domain = compute_domain(DOMAIN_BEACON_BUILDER, fork_version, genesis_validators_root)
/// ```
pub fn compute_epbs_domain(fork_version: [u8; 4], genesis_validators_root: B256) -> B256 {
    use ethereum_consensus::{
        phase0::beacon_state::ForkData,
        primitives::{Root, Version},
        ssz::prelude::*,
    };

    let version = Version::try_from(fork_version.as_slice()).expect("fork_version is 4 bytes");
    let root = Root::try_from(genesis_validators_root.as_slice()).expect("root is 32 bytes");

    let fork_data = ForkData {
        current_version: version,
        genesis_validators_root: root,
    };

    let fork_data_root = fork_data
        .hash_tree_root()
        .expect("ForkData hash_tree_root should not fail");

    // construcrt domain: DOMAIN_BEACON_BUILDER || fork_data_root[:28]
    let mut domain = [0u8; 32];
    domain[0..4].copy_from_slice(&DOMAIN_BEACON_BUILDER);
    domain[4..32].copy_from_slice(&fork_data_root[..28]);

    B256::from(domain)
}
