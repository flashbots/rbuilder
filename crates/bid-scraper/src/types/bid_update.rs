use alloy_primitives::{Address, BlockHash, U256};
use alloy_rpc_types_beacon::BlsPublicKey;
use ssz_derive::Decode;

#[derive(Debug, Decode)]
pub struct TopBidUpdate {
    /// Millisecond timestamp at which this became the top bid
    pub timestamp: u64,
    pub slot: u64,
    pub block_number: u64,
    pub block_hash: BlockHash,
    pub parent_hash: BlockHash,
    pub builder_pubkey: BlsPublicKey,
    pub fee_recipient: Address,
    pub value: U256,
}
