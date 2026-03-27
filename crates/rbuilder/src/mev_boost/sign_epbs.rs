//! EPBS bid and envelope signing for EIP-7732/Gloas.
//!
//! This module implements signing for ExecutionPayloadBid and ExecutionPayloadEnvelope
//! using the DOMAIN_BEACON_BUILDER domain as specified in the consensus specs.

use alloy_primitives::{Address, BlockHash, B256};
use alloy_rpc_types_beacon::BlsSignature;
use ethereum_consensus::{
    bellatrix::Transaction,
    capella::Withdrawal,
    crypto::SecretKey,
    primitives::{Bytes32, ExecutionAddress, Gwei, Hash32},
    signing::sign_with_domain,
    ssz::prelude::*,
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

    /// Sign an ExecutionPayloadBid.
    ///
    /// This follows the spec:
    /// ```python
    /// def get_execution_payload_bid_signature(
    ///     state: BeaconState, bid: ExecutionPayloadBid, privkey: int
    /// ) -> BLSSignature
    pub fn sign_bid(&self, bid: &ExecutionPayloadBid) -> eyre::Result<SignedExecutionPayloadBid> {
        let ssz_bid = SszExecutionPayloadBid::from_bid(bid);
        let signature = sign_with_domain(&ssz_bid, &self.sec, *self.domain)?;
        let signature = BlsSignature::from_slice(&signature);

        Ok(SignedExecutionPayloadBid {
            message: bid.clone(),
            signature,
        })
    }

    pub fn sign_envelope(
        &self,
        envelope: &ExecutionPayloadEnvelope,
    ) -> eyre::Result<SignedExecutionPayloadEnvelope> {
        let ssz_envelope = SszExecutionPayloadEnvelope::from_envelope(envelope)?;
        let signature = sign_with_domain(&ssz_envelope, &self.sec, *self.domain)?;
        let signature = BlsSignature::from_slice(&signature);

        Ok(SignedExecutionPayloadEnvelope {
            message: envelope.clone(),
            signature,
        })
    }
}

/// SSZ-merkleizable version of ExecutionPayloadBid for signing.

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

// TODO: use a better approach here. Import types when available rather
impl SszExecutionPayloadBid {
    pub fn from_bid(bid: &ExecutionPayloadBid) -> Self {
        let commitments_refs: Vec<&[u8]> = bid
            .blob_kzg_commitments
            .iter()
            .map(|c| c.as_ref())
            .collect();
        let commitments_root =
            rbuilder_primitives::mev_boost::ssz_roots::calculate_blob_kzg_commitments_root_ssz(
                &commitments_refs,
            );

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
            blob_kzg_commitments_root: hash32_from_b256(&commitments_root),
        }
    }
}

// mainnet constants from consensus-specs
const BYTES_PER_LOGS_BLOOM: usize = 256;
const MAX_EXTRA_DATA_BYTES: usize = 32;
const MAX_BYTES_PER_TRANSACTION: usize = 1_073_741_824; // 2^30
const MAX_TRANSACTIONS_PER_PAYLOAD: usize = 1_048_576; // 2^20
const MAX_WITHDRAWALS_PER_PAYLOAD: usize = 16;
const MAX_DEPOSIT_REQUESTS_PER_PAYLOAD: usize = 8192; // 2^13
const MAX_WITHDRAWAL_REQUESTS_PER_PAYLOAD: usize = 16; // 2^4
const MAX_CONSOLIDATION_REQUESTS_PER_PAYLOAD: usize = 2; // 2^1

// TODO: import via libs when available
/// SSZ `DepositRequest` from Electra.
#[derive(Default, Debug, Clone, PartialEq, Eq, SimpleSerialize)]
pub struct SszDepositRequest {
    pub pubkey: ByteVector<48>,
    pub withdrawal_credentials: Bytes32,
    pub amount: u64,
    pub signature: ByteVector<96>,
    pub index: u64,
}

// TODO: import via libs when available
/// SSZ `WithdrawalRequest` from Electra.
#[derive(Default, Debug, Clone, PartialEq, Eq, SimpleSerialize)]
pub struct SszWithdrawalRequest {
    pub source_address: ExecutionAddress,
    pub validator_pubkey: ByteVector<48>,
    pub amount: u64,
}

// TODO: import via libs when available
/// SSZ `ConsolidationRequest` from Electra.
#[derive(Default, Debug, Clone, PartialEq, Eq, SimpleSerialize)]
pub struct SszConsolidationRequest {
    pub source_address: ExecutionAddress,
    pub source_pubkey: ByteVector<48>,
    pub target_pubkey: ByteVector<48>,
}

// TODO: import via libs when available
/// SSZ `ExecutionRequests` from Electra.
#[derive(Default, Debug, Clone, PartialEq, Eq, SimpleSerialize)]
pub struct SszExecutionRequests {
    pub deposits: List<SszDepositRequest, MAX_DEPOSIT_REQUESTS_PER_PAYLOAD>,
    pub withdrawals: List<SszWithdrawalRequest, MAX_WITHDRAWAL_REQUESTS_PER_PAYLOAD>,
    pub consolidations: List<SszConsolidationRequest, MAX_CONSOLIDATION_REQUESTS_PER_PAYLOAD>,
}

/// SSZ `ExecutionPayload`
#[derive(Default, Debug, Clone, PartialEq, Eq, SimpleSerialize)]
pub struct SszExecutionPayload {
    pub parent_hash: Hash32,
    pub fee_recipient: ExecutionAddress,
    pub state_root: Bytes32,
    pub receipts_root: Bytes32,
    pub logs_bloom: ByteVector<BYTES_PER_LOGS_BLOOM>,
    pub prev_randao: Bytes32,
    pub block_number: u64,
    pub gas_limit: u64,
    pub gas_used: u64,
    pub timestamp: u64,
    pub extra_data: ByteList<MAX_EXTRA_DATA_BYTES>,
    pub base_fee_per_gas: U256,
    pub block_hash: Hash32,
    pub transactions:
        List<Transaction<MAX_BYTES_PER_TRANSACTION>, MAX_TRANSACTIONS_PER_PAYLOAD>,
    pub withdrawals: List<Withdrawal, MAX_WITHDRAWALS_PER_PAYLOAD>,
    pub blob_gas_used: u64,
    pub excess_blob_gas: u64,
}

// TODO: import via libs when available
/// SSZ `ExecutionPayloadEnvelope` from Gloas.
#[derive(Default, Debug, Clone, PartialEq, Eq, SimpleSerialize)]
pub struct SszExecutionPayloadEnvelope {
    pub payload: SszExecutionPayload,
    pub execution_requests: SszExecutionRequests,
    pub builder_index: u64,
    pub beacon_block_root: Hash32,
    pub slot: u64,
    pub state_root: Hash32,
}

impl SszExecutionPayloadEnvelope {
    pub fn from_envelope(
        envelope: &ExecutionPayloadEnvelope,
    ) -> eyre::Result<Self> {
        let inner1 = &envelope.payload.payload_inner.payload_inner;
        let inner2 = &envelope.payload.payload_inner;
        let inner3 = &envelope.payload;

        // convert transactions
        let mut transactions = List::default();
        for tx_bytes in &inner1.transactions {
            let tx = Transaction::try_from(tx_bytes.as_ref())
                .map_err(|e| eyre::eyre!("Failed to convert transaction: {:?}", e))?;
            transactions.push(tx);
        }

        // convert withdrawals
        let mut withdrawals = List::default();
        for w in &inner2.withdrawals {
            let withdrawal = Withdrawal {
                index: w.index as usize,
                validator_index: w.validator_index as usize,
                address: ExecutionAddress::try_from(w.address.as_slice())
                    .expect("Address is 20 bytes"),
                amount: w.amount as Gwei,
            };
            withdrawals.push(withdrawal);
        }

        // convert extra_data
        let extra_data = ByteList::try_from(inner1.extra_data.as_ref())
            .map_err(|e| eyre::eyre!("Extra data too long: {:?}", e))?;

        let payload = SszExecutionPayload {
            parent_hash: hash32_from_b256(&B256::from(inner1.parent_hash)),
            fee_recipient: ExecutionAddress::try_from(inner1.fee_recipient.as_slice())
                .expect("Address is 20 bytes"),
            state_root: bytes32_from_b256(&B256::from(inner1.state_root)),
            receipts_root: bytes32_from_b256(&B256::from(inner1.receipts_root)),
            logs_bloom: ByteVector::try_from(inner1.logs_bloom.as_ref())
                .map_err(|e| eyre::eyre!("Invalid logs_bloom: {:?}", e))?,
            prev_randao: bytes32_from_b256(&B256::from(inner1.prev_randao)),
            block_number: inner1.block_number,
            gas_limit: inner1.gas_limit,
            gas_used: inner1.gas_used,
            timestamp: inner1.timestamp,
            extra_data,
            base_fee_per_gas: inner1.base_fee_per_gas,
            block_hash: hash32_from_b256(&B256::from(inner1.block_hash)),
            transactions,
            withdrawals,
            blob_gas_used: inner3.blob_gas_used,
            excess_blob_gas: inner3.excess_blob_gas,
        };

        // convert execution requests
        let execution_requests =
            convert_execution_requests_to_ssz(&envelope.execution_requests)?;

        Ok(Self {
            payload,
            execution_requests,
            builder_index: envelope.builder_index,
            beacon_block_root: hash32_from_b256(&envelope.beacon_block_root),
            slot: envelope.slot,
            state_root: hash32_from_b256(&envelope.state_root),
        })
    }
}

/// Convert our raw-bytes ExecutionRequests to proper SSZ typed requests.
fn convert_execution_requests_to_ssz(
    requests: &rbuilder_primitives::epbs::ExecutionRequests,
) -> eyre::Result<SszExecutionRequests> {
    let mut ssz_requests = SszExecutionRequests::default();

    for raw in &requests.deposits {
        if raw.len() < 192 {
            continue; // skip malformed
        }
        let req = SszDepositRequest {
            pubkey: ByteVector::try_from(&raw[0..48])
                .map_err(|e| eyre::eyre!("deposit pubkey: {:?}", e))?,
            withdrawal_credentials: Bytes32::try_from(&raw[48..80])
                .map_err(|e| eyre::eyre!("deposit withdrawal_credentials: {:?}", e))?,
            amount: u64::from_le_bytes(raw[80..88].try_into().unwrap()),
            signature: ByteVector::try_from(&raw[88..184])
                .map_err(|e| eyre::eyre!("deposit signature: {:?}", e))?,
            index: u64::from_le_bytes(raw[184..192].try_into().unwrap()),
        };
        ssz_requests.deposits.push(req);
    }

    for raw in &requests.withdrawals {
        if raw.len() < 76 {
            continue;
        }
        let req = SszWithdrawalRequest {
            source_address: ExecutionAddress::try_from(&raw[0..20])
                .map_err(|e| eyre::eyre!("withdrawal source_address: {:?}", e))?,
            validator_pubkey: ByteVector::try_from(&raw[20..68])
                .map_err(|e| eyre::eyre!("withdrawal validator_pubkey: {:?}", e))?,
            amount: u64::from_le_bytes(raw[68..76].try_into().unwrap()),
        };
        ssz_requests.withdrawals.push(req);
    }

    for raw in &requests.consolidations {
        if raw.len() < 116 {
            continue;
        }
        let req = SszConsolidationRequest {
            source_address: ExecutionAddress::try_from(&raw[0..20])
                .map_err(|e| eyre::eyre!("consolidation source_address: {:?}", e))?,
            source_pubkey: ByteVector::try_from(&raw[20..68])
                .map_err(|e| eyre::eyre!("consolidation source_pubkey: {:?}", e))?,
            target_pubkey: ByteVector::try_from(&raw[68..116])
                .map_err(|e| eyre::eyre!("consolidation target_pubkey: {:?}", e))?,
        };
        ssz_requests.consolidations.push(req);
    }

    Ok(ssz_requests)
}

// Helper conversion functions

fn hash32_from_block_hash(h: &BlockHash) -> Hash32 {
    Hash32::try_from(h.as_slice()).expect("BlockHash is 32 bytes")
}

fn hash32_from_b256(h: &B256) -> Hash32 {
    Hash32::try_from(h.as_slice()).expect("B256 is 32 bytes")
}

fn bytes32_from_b256(h: &B256) -> Bytes32 {
    Bytes32::try_from(h.as_slice()).expect("B256 is 32 bytes")
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
pub fn compute_epbs_domain(fork_version: [u8; 4], genesis_validators_root: B256) -> B256 {
    use ethereum_consensus::{
        phase0::beacon_state::ForkData,
        primitives::{Root, Version},
        ssz::prelude::*,
    };

    // create ForkData and compute its hash_tree_root
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