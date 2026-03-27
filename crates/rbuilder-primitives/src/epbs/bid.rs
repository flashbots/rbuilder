//! ExecutionPayloadBid types for EPBS.
//!
//! These types represent the builder's commitment to produce an execution payload.
//! See: https://github.com/ethereum/consensus-specs/blob/master/specs/gloas/builder.md

use alloy_primitives::{Address, BlockHash, Bytes, B256};
use alloy_rpc_types_beacon::BlsSignature;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

/// Signing domain for EPBS builder bids.
/// From consensus-specs/specs/gloas/beacon-chain.md:
/// | DOMAIN_BEACON_BUILDER | DomainType('0x0B000000') |
pub const DOMAIN_BEACON_BUILDER: [u8; 4] = [0x0B, 0x00, 0x00, 0x00];

/// from consensus-specs/specs/gloas/beacon-chain.md:
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExecutionPayloadBid {
    /// hash of the current head of execution chain
    pub parent_block_hash: BlockHash,
    /// hash tree root of the beacon block the proposer will build on
    pub parent_block_root: B256,
    /// this is the blockhash which the builder constructed the payload
    pub block_hash: BlockHash,
    /// previous RANDAO of the constructed payload
    pub prev_randao: B256,
    /// execution address to receive the payment
    pub fee_recipient: Address,
    /// gas limit of the constructed payload
    #[serde_as(as = "DisplayFromStr")]
    pub gas_limit: u64,
    /// validator index of the builder performing these actions.
    #[serde_as(as = "DisplayFromStr")]
    pub builder_index: u64,
    /// to be the slot for which this bid is aimed.
    #[serde_as(as = "DisplayFromStr")]
    pub slot: u64,
    /// to be the value (in gwei) that the builder will pay the proposer if the bid is accepted
    #[serde_as(as = "DisplayFromStr")]
    pub value: u64,
    /// must be zero for in protocol payments. non-zero only if proposer accepts trusted payments
    #[serde_as(as = "DisplayFromStr")]
    pub execution_payment: u64,
    /// blob commitments for the payload.
    pub blob_kzg_commitments: Vec<Bytes>,
}

impl ExecutionPayloadBid {
    /// Returns the total payment to the proposer (value + execution_payment).
    pub fn total_value(&self) -> u64 {
        self.value.saturating_add(self.execution_payment)
    }

    /// Returns true if this bid uses only in-protocol (beacon chain) payment.
    pub fn is_in_protocol_payment(&self) -> bool {
        self.execution_payment == 0
    }
}

/// SignedExecutionPayloadBid is a signed commitment from a builder.
///
/// signature is created using the builder's validator key and the
/// DOMAIN_BEACON_BUILDER domain.
///
/// from consensus-specs/specs/gloas/beacon-chain.md:

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SignedExecutionPayloadBid {
    /// execution payload
    pub message: ExecutionPayloadBid,
    /// bls signature over the bid using the builder's validator key.
    pub signature: BlsSignature,
}

/// resp for get_bid endpoint.
///
/// This follows the Builder API spec for EPBS:
/// GET /eth/v1/builder/execution_payload_bid/{slot}/{parent_hash}/{parent_root}/{proposer_index}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetExecutionPayloadBidResponse {
    /// The fork version, e.g., "gloas".
    pub version: String,
    /// signed bid using validator signature
    pub data: SignedExecutionPayloadBid,
}

/// the params are for the get_bid endpoint following the builder-sepc
#[derive(Debug, Clone)]
pub struct GetBidParams {
    /// slot for which the bid is being considered for
    pub slot: u64,
    /// hash of the parent block the proposer will upon
    pub parent_hash: BlockHash,
    /// root of the parent block the proposer will build upon
    pub parent_root: B256,
    /// to be reitrved from the path params
    pub proposer_index: u64,
    /// address from the X-Fee-Recipient header
    pub fee_recipient: Address,
    /// timeout ms for request via X-Timeout-Ms header
    pub timeout_ms: Option<u64>,
    /// timestamp from Date-Milliseconds header for latency measurement
    pub date_milliseconds: Option<u64>,
}