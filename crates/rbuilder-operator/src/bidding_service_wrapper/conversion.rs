//! Conversion real data <-> gRPC data
use crate::bidding_service_wrapper::{
    LandedBlockInfo as RPCLandedBlockInfo, RelaySet as RPCRelaySet,
};

use alloy_primitives::{BlockHash, U256};
use rbuilder::live_builder::block_output::bidding_service_interface::{
    LandedBlockInfo as RealLandedBlockInfo, RelaySet as RealRelaySet,
};
use time::OffsetDateTime;
use tonic::Status;

pub fn real2rpc_landed_block_info(l: &RealLandedBlockInfo) -> RPCLandedBlockInfo {
    RPCLandedBlockInfo {
        block_number: l.block_number,
        block_timestamp: l.block_timestamp.unix_timestamp(),
        builder_balance: l.builder_balance.as_limbs().to_vec(),
        beneficiary_is_builder: l.beneficiary_is_builder,
    }
}

#[allow(clippy::result_large_err)]
pub fn rpc2real_landed_block_info(l: &RPCLandedBlockInfo) -> Result<RealLandedBlockInfo, Status> {
    Ok(RealLandedBlockInfo {
        block_number: l.block_number,
        block_timestamp: OffsetDateTime::from_unix_timestamp(l.block_timestamp)
            .map_err(|_| Status::invalid_argument("block_timestamp"))?,
        builder_balance: U256::from_limbs_slice(&l.builder_balance),
        beneficiary_is_builder: l.beneficiary_is_builder,
    })
}

pub fn real2rpc_u256(v: U256) -> Vec<u64> {
    v.as_limbs().to_vec()
}

#[allow(clippy::result_large_err)]
pub fn rpc2real_u256(v: Vec<u64>) -> Result<U256, Status> {
    U256::checked_from_limbs_slice(&v).ok_or(Status::invalid_argument("rpc U256 limbs error"))
}

pub fn real2rpc_block_hash(v: BlockHash) -> Vec<u8> {
    v.as_slice().to_vec()
}

#[allow(clippy::result_large_err)]
pub fn rpc2real_block_hash(v: &Vec<u8>) -> Result<BlockHash, Status> {
    BlockHash::try_from(v.as_slice()).map_err(|_| Status::invalid_argument("rpc BlockHash error"))
}

pub fn real2rpc_relay_set(l: &RealRelaySet) -> RPCRelaySet {
    RPCRelaySet {
        relay_ids: l.relays().to_vec(),
    }
}

pub fn rpc2real_relay_set(l: &RPCRelaySet) -> RealRelaySet {
    RealRelaySet::new(l.relay_ids.clone())
}
