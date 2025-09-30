//! Conversion real data <-> rpc data
use crate::bidding_service_wrapper::{LandedBlockInfo as RPCLandedBlockInfo, UpdateNewBidParams};

use alloy_primitives::{Address, BlockHash, U256};
use alloy_rpc_types_beacon::BlsPublicKey;
use bid_scraper::types::ScrapedRelayBlockBid;
use rbuilder::{
    live_builder::block_output::bidding_service_interface::{
        LandedBlockInfo as RealLandedBlockInfo, ScrapedRelayBlockBidWithStats,
    },
    utils::{offset_datetime_to_timestamp_us, timestamp_us_to_offset_datetime},
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

pub fn real2rpc_address(v: Address) -> Vec<u8> {
    v.as_slice().to_vec()
}

#[allow(clippy::result_large_err)]
pub fn rpc2real_address(v: Vec<u8>) -> Result<Address, Status> {
    Address::try_from(v.as_slice()).map_err(|_| Status::invalid_argument("rpc Address error"))
}

pub fn real2rpc_bls_public_key(v: BlsPublicKey) -> Vec<u8> {
    v.as_slice().to_vec()
}

#[allow(clippy::result_large_err)]
pub fn rpc2real_bls_public_key(v: Vec<u8>) -> Result<BlsPublicKey, Status> {
    BlsPublicKey::try_from(v.as_slice())
        .map_err(|_| Status::invalid_argument("rpc BlsPublicKey error"))
}

pub fn real2rpc_block_hash(v: BlockHash) -> Vec<u8> {
    v.as_slice().to_vec()
}

#[allow(clippy::result_large_err)]
pub fn rpc2real_block_hash(v: &Vec<u8>) -> Result<BlockHash, Status> {
    BlockHash::try_from(v.as_slice()).map_err(|_| Status::invalid_argument("rpc BlockHash error"))
}

pub fn real2rpc_block_bid(bid_with_stats: ScrapedRelayBlockBidWithStats) -> UpdateNewBidParams {
    let creation_time_us = offset_datetime_to_timestamp_us(bid_with_stats.creation_time);
    let bid = bid_with_stats.bid;
    UpdateNewBidParams {
        seen_time: bid.seen_time,
        publisher_name: bid.publisher_name,
        publisher_type: real2rpc_publisher_type(bid.publisher_type),
        relay_time: bid.relay_time,
        relay_name: bid.relay_name,
        block_hash: real2rpc_block_hash(bid.block_hash),
        parent_hash: real2rpc_block_hash(bid.parent_hash),
        value: real2rpc_u256(bid.value),
        slot_number: bid.slot_number,
        block_number: bid.block_number,
        builder_pubkey: bid
            .builder_pubkey
            .map(real2rpc_bls_public_key)
            .unwrap_or_default(),
        extra_data: bid.extra_data,
        fee_recipient: bid.fee_recipient.map(real2rpc_address).unwrap_or_default(),
        proposer_fee_recipient: bid
            .proposer_fee_recipient
            .map(real2rpc_address)
            .unwrap_or_default(),
        gas_used: bid.gas_used,
        optimistic_submission: bid.optimistic_submission,
        creation_time_us,
    }
}

#[allow(clippy::result_large_err)]
pub fn rpc2real_block_bid(
    bid: UpdateNewBidParams,
) -> Result<ScrapedRelayBlockBidWithStats, Status> {
    Ok(ScrapedRelayBlockBidWithStats::new_for_deserialization(
        ScrapedRelayBlockBid {
            seen_time: bid.seen_time,
            publisher_name: bid.publisher_name,
            publisher_type: rpc2real_publisher_type(bid.publisher_type)?,
            relay_time: bid.relay_time,
            relay_name: bid.relay_name,
            block_hash: rpc2real_block_hash(&bid.block_hash)?,
            parent_hash: rpc2real_block_hash(&bid.parent_hash)?,
            value: rpc2real_u256(bid.value)?,
            slot_number: bid.slot_number,
            block_number: bid.block_number,
            builder_pubkey: if bid.builder_pubkey.is_empty() {
                None
            } else {
                Some(rpc2real_bls_public_key(bid.builder_pubkey)?)
            },
            extra_data: bid.extra_data,
            fee_recipient: if bid.fee_recipient.is_empty() {
                None
            } else {
                Some(rpc2real_address(bid.fee_recipient)?)
            },
            proposer_fee_recipient: if bid.proposer_fee_recipient.is_empty() {
                None
            } else {
                Some(rpc2real_address(bid.proposer_fee_recipient)?)
            },
            gas_used: bid.gas_used,
            optimistic_submission: bid.optimistic_submission,
        },
        timestamp_us_to_offset_datetime(bid.creation_time_us),
    ))
}

pub fn real2rpc_publisher_type(ty: bid_scraper::types::PublisherType) -> i32 {
    match ty {
        bid_scraper::types::PublisherType::RelayBids => super::PublisherType::RelayBids as i32,
        bid_scraper::types::PublisherType::RelayHeaders => {
            super::PublisherType::RelayHeaders as i32
        }
        bid_scraper::types::PublisherType::UltrasoundWs => {
            super::PublisherType::UltrasoundWs as i32
        }
        bid_scraper::types::PublisherType::BloxrouteWs => super::PublisherType::BloxrouteWs as i32,
        bid_scraper::types::PublisherType::ExternalWs => super::PublisherType::ExternalWs as i32,
    }
}

#[allow(clippy::result_large_err)]
pub fn rpc2real_publisher_type(ty: i32) -> Result<bid_scraper::types::PublisherType, Status> {
    if let Some(ty) = super::PublisherType::from_i32(ty) {
        Ok(match ty {
            super::PublisherType::RelayBids => bid_scraper::types::PublisherType::RelayBids,
            super::PublisherType::RelayHeaders => bid_scraper::types::PublisherType::RelayHeaders,
            super::PublisherType::UltrasoundWs => bid_scraper::types::PublisherType::UltrasoundWs,
            super::PublisherType::BloxrouteWs => bid_scraper::types::PublisherType::BloxrouteWs,
            super::PublisherType::ExternalWs => bid_scraper::types::PublisherType::ExternalWs,
        })
    } else {
        Err(Status::invalid_argument("rpc PublisherType error"))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, BlockHash, U256};
    use alloy_rpc_types_beacon::BlsPublicKey;
    use bid_scraper::types::ScrapedRelayBlockBid;
    use rbuilder::{
        live_builder::block_output::bidding_service_interface::ScrapedRelayBlockBidWithStats,
        utils::timestamp_ms_to_offset_datetime,
    };
    use std::str::FromStr;

    use crate::bidding_service_wrapper::conversion::{real2rpc_block_bid, rpc2real_block_bid};

    fn test_roundtrip(bid: ScrapedRelayBlockBid) {
        let bid_with_stats = ScrapedRelayBlockBidWithStats::new_for_deserialization(
            bid,
            timestamp_ms_to_offset_datetime(1000),
        );
        let rpc_bid = real2rpc_block_bid(bid_with_stats.clone());
        assert_eq!(rpc2real_block_bid(rpc_bid).unwrap(), bid_with_stats);
    }

    #[test]
    /// Test all with all options as Some
    fn test_block_bid_conversion_some() {
        let bid = ScrapedRelayBlockBid {
            seen_time: 1234.0,
            publisher_name: "Mafalda".to_owned(),
            publisher_type: bid_scraper::types::PublisherType::BloxrouteWs,
            relay_time: Some(2345.6),
            relay_name: "Flashbots".to_owned(),
            block_hash: BlockHash::from_str(
                "0xe57c063ad96fb5b6fe7696dc8509f3a986ace89d06a19951f3e4404f877bb0ca",
            )
            .unwrap(),
            parent_hash: BlockHash::from_str(
                "0xf2ae3ad64c285ab1de2195f23c19b2b2dcf4949b6f71a4a3406bac9734e1ff27",
            )
            .unwrap(),
            value: U256::from(876543210),
            slot_number: 31415,
            block_number: 27182,
            builder_pubkey: Some(BlsPublicKey::from_str("0xf2ae3ad64c285ab1de2195f23c19b2b2dcf4949b6f71a4a3406bac9734e1ff2701234567890123456789012345678901").unwrap()),
            extra_data: Some("extra_data!".to_owned()),
            fee_recipient: Some(address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266")),
            proposer_fee_recipient: Some(address!("1234d6e51aad88F6F4ce6aB8827279cffFb92266")),
            gas_used: Some(666),
            optimistic_submission: Some(true),
        };
        test_roundtrip(bid);
    }

    #[test]

    /// Test all with all options as None
    fn test_block_bid_conversion_none() {
        let bid = ScrapedRelayBlockBid {
            seen_time: 1234.0,
            publisher_name: "".to_owned(),
            publisher_type: bid_scraper::types::PublisherType::BloxrouteWs,
            relay_time: None,
            relay_name: "".to_owned(),
            block_hash: BlockHash::from_str(
                "0xe57c063ad96fb5b6fe7696dc8509f3a986ace89d06a19951f3e4404f877bb0ca",
            )
            .unwrap(),
            parent_hash: BlockHash::from_str(
                "0xf2ae3ad64c285ab1de2195f23c19b2b2dcf4949b6f71a4a3406bac9734e1ff27",
            )
            .unwrap(),
            value: U256::from(876543210),
            slot_number: 31415,
            block_number: 27182,
            builder_pubkey: None,
            extra_data: None,
            fee_recipient: None,
            proposer_fee_recipient: None,
            gas_used: None,
            optimistic_submission: None,
        };
        test_roundtrip(bid);
    }
}
