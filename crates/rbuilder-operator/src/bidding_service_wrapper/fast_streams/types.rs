use alloy_primitives::U256;
use bid_scraper::types::ScrapedRelayBlockBid;
use iceoryx2::prelude::ZeroCopySend;
use iceoryx2_bb_container::byte_string::FixedSizeByteString;
use rbuilder::{
    building::builders::BuiltBlockId,
    live_builder::block_output::bidding_service_interface::{
        BuiltBlockDescriptorForSlotBidder, ScrapedRelayBlockBidWithStats,
    },
    utils::{offset_datetime_to_timestamp_us, timestamp_us_to_offset_datetime},
};

/// Used sometimes to generalize some latency code checks.
pub trait WithCreationTime {
    fn creation_time_us(&self) -> u64;
}

#[derive(Debug, Clone, Copy, ZeroCopySend)]
#[repr(C)]
pub enum PublisherType {
    RelayBids = 0,
    RelayHeaders = 1,
    UltrasoundWs = 2,
    BloxrouteWs = 3,
    ExternalWs = 4,
}

impl From<bid_scraper::types::PublisherType> for PublisherType {
    fn from(publisher_type: bid_scraper::types::PublisherType) -> Self {
        match publisher_type {
            bid_scraper::types::PublisherType::RelayBids => PublisherType::RelayBids,
            bid_scraper::types::PublisherType::RelayHeaders => PublisherType::RelayHeaders,
            bid_scraper::types::PublisherType::UltrasoundWs => PublisherType::UltrasoundWs,
            bid_scraper::types::PublisherType::BloxrouteWs => PublisherType::BloxrouteWs,
            bid_scraper::types::PublisherType::ExternalWs => PublisherType::ExternalWs,
        }
    }
}

impl Into<bid_scraper::types::PublisherType> for PublisherType {
    fn into(self) -> bid_scraper::types::PublisherType {
        match self {
            PublisherType::RelayBids => bid_scraper::types::PublisherType::RelayBids,
            PublisherType::RelayHeaders => bid_scraper::types::PublisherType::RelayHeaders,
            PublisherType::UltrasoundWs => bid_scraper::types::PublisherType::UltrasoundWs,
            PublisherType::BloxrouteWs => bid_scraper::types::PublisherType::BloxrouteWs,
            PublisherType::ExternalWs => bid_scraper::types::PublisherType::ExternalWs,
        }
    }
}

const MAX_RELAY_NAME_LENGTH: usize = 100;
const MAX_PUBLISHER_NAME_LENGTH: usize = 100;
const MAX_EXTRA_DATA_LENGTH: usize = 32;
const ADDRESS_DATA_LENGTH: usize = 20;
const HASH_DATA_LENGTH: usize = 32;
const U256_DATA_LENGTH: usize = 32;
const BLS_KEY_DATA_LENGTH: usize = 48;

/// Vesion of bid_scraper::types::bid::ScrapedRelayBlockBidWithStats compatible with ZeroCopySend

#[derive(Debug, Clone, Copy, ZeroCopySend)]
#[type_name("ScrapedRelayBlockBidRPC")]
#[repr(C)]
pub struct ScrapedRelayBlockBidRPC {
    pub seen_time: f64,
    pub publisher_name: FixedSizeByteString<MAX_PUBLISHER_NAME_LENGTH>,
    pub publisher_type: PublisherType,
    pub relay_time: Option<f64>,
    pub relay_name: FixedSizeByteString<MAX_RELAY_NAME_LENGTH>,
    pub block_hash: [u8; HASH_DATA_LENGTH],
    pub parent_hash: [u8; HASH_DATA_LENGTH],
    pub value: [u8; U256_DATA_LENGTH],
    pub slot_number: u64,
    pub block_number: u64,
    pub builder_pubkey: Option<[u8; BLS_KEY_DATA_LENGTH]>,
    pub extra_data: Option<FixedSizeByteString<MAX_EXTRA_DATA_LENGTH>>,
    pub fee_recipient: Option<[u8; ADDRESS_DATA_LENGTH]>, // block COINBASE
    pub proposer_fee_recipient: Option<[u8; ADDRESS_DATA_LENGTH]>, // validator address
    pub gas_used: Option<u64>,
    pub optimistic_submission: Option<bool>,
    pub creation_time_us: u64,
}

impl WithCreationTime for ScrapedRelayBlockBidRPC {
    fn creation_time_us(&self) -> u64 {
        self.creation_time_us
    }
}

impl From<ScrapedRelayBlockBidWithStats> for ScrapedRelayBlockBidRPC {
    fn from(bid_with_stats: ScrapedRelayBlockBidWithStats) -> Self {
        let scraped_bid = bid_with_stats.bid;
        ScrapedRelayBlockBidRPC {
            seen_time: scraped_bid.seen_time,
            publisher_name: FixedSizeByteString::<MAX_PUBLISHER_NAME_LENGTH>::from_str_truncated(
                &scraped_bid.publisher_name,
            ),
            publisher_type: scraped_bid.publisher_type.into(),
            relay_time: scraped_bid.relay_time,
            relay_name: FixedSizeByteString::<MAX_RELAY_NAME_LENGTH>::from_str_truncated(
                &scraped_bid.relay_name,
            ),
            block_hash: scraped_bid.block_hash.into(),
            parent_hash: scraped_bid.parent_hash.into(),
            value: scraped_bid.value.to_le_bytes(),
            slot_number: scraped_bid.slot_number,
            block_number: scraped_bid.block_number,
            builder_pubkey: scraped_bid.builder_pubkey.map(|k| k.into()),
            extra_data: scraped_bid
                .extra_data
                .map(|k| FixedSizeByteString::<MAX_EXTRA_DATA_LENGTH>::from_str_truncated(&k)),
            fee_recipient: scraped_bid.fee_recipient.map(|k| k.into()),
            proposer_fee_recipient: scraped_bid.proposer_fee_recipient.map(|k| k.into()),
            gas_used: scraped_bid.gas_used,
            optimistic_submission: scraped_bid.optimistic_submission,
            creation_time_us: offset_datetime_to_timestamp_us(bid_with_stats.creation_time),
        }
    }
}

impl Into<ScrapedRelayBlockBidWithStats> for ScrapedRelayBlockBidRPC {
    fn into(self) -> ScrapedRelayBlockBidWithStats {
        let bid = ScrapedRelayBlockBid {
            seen_time: self.seen_time,
            publisher_name: self.publisher_name.to_string(),
            publisher_type: self.publisher_type.into(),
            relay_time: self.relay_time,
            relay_name: self.relay_name.to_string(),
            block_hash: self.block_hash.into(),
            parent_hash: self.parent_hash.into(),
            value: U256::from_le_bytes(self.value),
            slot_number: self.slot_number,
            block_number: self.block_number,
            builder_pubkey: self.builder_pubkey.map(|k| k.into()),
            extra_data: self.extra_data.map(|k| k.to_string()),
            fee_recipient: self.fee_recipient.map(|k| k.into()),
            proposer_fee_recipient: self.proposer_fee_recipient.map(|k| k.into()),
            gas_used: self.gas_used,
            optimistic_submission: self.optimistic_submission,
        };
        ScrapedRelayBlockBidWithStats {
            bid,
            creation_time: timestamp_us_to_offset_datetime(self.creation_time_us),
        }
    }
}

#[derive(Debug, Clone, Copy, ZeroCopySend)]
#[type_name("BuiltBlockDescriptorForSlotBidderRPC")]
#[repr(C)]

pub struct BuiltBlockDescriptorForSlotBidderRPC {
    pub session_id: u64,
    pub true_block_value: [u8; U256_DATA_LENGTH],
    pub block_id: u64,
    pub creation_time_us: u64,
}

impl WithCreationTime for BuiltBlockDescriptorForSlotBidderRPC {
    fn creation_time_us(&self) -> u64 {
        self.creation_time_us
    }
}

impl BuiltBlockDescriptorForSlotBidderRPC {
    pub fn new(session_id: u64, block: BuiltBlockDescriptorForSlotBidder) -> Self {
        Self {
            session_id,
            true_block_value: block.true_block_value.to_le_bytes(),
            block_id: block.id.0,
            creation_time_us: offset_datetime_to_timestamp_us(block.creation_time),
        }
    }
}

impl Into<BuiltBlockDescriptorForSlotBidder> for BuiltBlockDescriptorForSlotBidderRPC {
    fn into(self) -> BuiltBlockDescriptorForSlotBidder {
        BuiltBlockDescriptorForSlotBidder {
            true_block_value: U256::from_le_bytes(self.true_block_value),
            id: BuiltBlockId(self.block_id),
            creation_time: timestamp_us_to_offset_datetime(self.creation_time_us),
        }
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

    use crate::bidding_service_wrapper::fast_streams::types::ScrapedRelayBlockBidRPC;

    fn test_roundtrip(bid: ScrapedRelayBlockBid) {
        let bid_with_stats = ScrapedRelayBlockBidWithStats::new_for_deserialization(
            bid,
            timestamp_ms_to_offset_datetime(1000),
        );
        let rpc_bid = ScrapedRelayBlockBidRPC::from(bid_with_stats.clone());
        let rpc_bid_back: ScrapedRelayBlockBidWithStats = rpc_bid.into();

        assert_eq!(rpc_bid_back, bid_with_stats);
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
