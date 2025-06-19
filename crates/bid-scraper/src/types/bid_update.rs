use ethers::prelude::*;
use ssz_derive::Decode;
use ssz_types::{typenum, FixedVector};

/// Top bid update published by ultrasound and titan relays.
#[derive(Debug, Decode)]
pub struct TopBidUpdate {
    /// Millisecond timestamp at which this became the top bid
    pub timestamp: u64,
    pub slot: u64,
    pub block_number: u64,
    pub block_hash: H256,
    pub parent_hash: H256,
    pub builder_pubkey: FixedVector<u8, typenum::U48>,
    pub fee_recipient: Address,
    pub value: U256,
}
