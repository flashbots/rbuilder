//! Types used to communicate with the bidding service via iceoryx.
//! They are mostly mappings of the originals but supporting ZeroCopySend.
use alloy_primitives::{I256, U256};
use bid_scraper::types::ScrapedRelayBlockBid;
use iceoryx2::prelude::ZeroCopySend;
use iceoryx2_bb_container::byte_string::FixedSizeByteString;
use rbuilder::{
    building::builders::BuiltBlockId,
    live_builder::block_output::bidding_service_interface::{
        BuiltBlockDescriptorForSlotBidder, PayoutInfo, RelaySet, ScrapedRelayBlockBidWithStats,
        SlotBidderSealBidCommand,
    },
    utils::{offset_datetime_to_timestamp_us, timestamp_us_to_offset_datetime},
};
use tracing::error;

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

impl From<PublisherType> for bid_scraper::types::PublisherType {
    fn from(val: PublisherType) -> Self {
        match val {
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
/// In practice we will never have more than a few (2).
const MAX_RELAY_SETS_COUNT: usize = 10;
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

impl From<ScrapedRelayBlockBidRPC> for ScrapedRelayBlockBidWithStats {
    fn from(val: ScrapedRelayBlockBidRPC) -> Self {
        let bid = ScrapedRelayBlockBid {
            seen_time: val.seen_time,
            publisher_name: val.publisher_name.to_string(),
            publisher_type: val.publisher_type.into(),
            relay_time: val.relay_time,
            relay_name: val.relay_name.to_string(),
            block_hash: val.block_hash.into(),
            parent_hash: val.parent_hash.into(),
            value: U256::from_le_bytes(val.value),
            slot_number: val.slot_number,
            block_number: val.block_number,
            builder_pubkey: val.builder_pubkey.map(|k| k.into()),
            extra_data: val.extra_data.map(|k| k.to_string()),
            fee_recipient: val.fee_recipient.map(|k| k.into()),
            proposer_fee_recipient: val.proposer_fee_recipient.map(|k| k.into()),
            gas_used: val.gas_used,
            optimistic_submission: val.optimistic_submission,
        };
        ScrapedRelayBlockBidWithStats {
            bid,
            creation_time: timestamp_us_to_offset_datetime(val.creation_time_us),
        }
    }
}

pub type BuiltBlockDescriptorForSlotBidderWithSessionId = (BuiltBlockDescriptorForSlotBidder, u64);

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

impl From<BuiltBlockDescriptorForSlotBidderWithSessionId> for BuiltBlockDescriptorForSlotBidderRPC {
    fn from(value: BuiltBlockDescriptorForSlotBidderWithSessionId) -> Self {
        Self {
            session_id: value.1,
            true_block_value: value.0.true_block_value.to_le_bytes(),
            block_id: value.0.id.0,
            creation_time_us: offset_datetime_to_timestamp_us(value.0.creation_time),
        }
    }
}

impl From<BuiltBlockDescriptorForSlotBidderRPC> for BuiltBlockDescriptorForSlotBidder {
    fn from(val: BuiltBlockDescriptorForSlotBidderRPC) -> Self {
        BuiltBlockDescriptorForSlotBidder {
            true_block_value: U256::from_le_bytes(val.true_block_value),
            id: BuiltBlockId(val.block_id),
            creation_time: timestamp_us_to_offset_datetime(val.creation_time_us),
        }
    }
}

pub type SlotBidderSealBidCommandWithSessionId = (SlotBidderSealBidCommand, u64);

#[derive(Debug, Copy, Clone, ZeroCopySend, Default)]
#[type_name("PayoutInfoRPC")]
#[repr(C)]
pub struct PayoutInfoRPC {
    /// Index of the relay set returned by the bidding service on initialize.
    pub relay_set_index: usize,
    pub payout_tx_value: [u8; U256_DATA_LENGTH],
    pub subsidy: [u8; U256_DATA_LENGTH],
}

impl PayoutInfoRPC {
    /// If it fails to find the relay set, returns None.
    /// relay_sets should be the same as the ones returned by the bidding service on initialize.
    fn try_from(value: PayoutInfo, relay_sets: &[RelaySet]) -> Option<Self> {
        let relay_set_index = relay_sets.iter().position(|r| r == &value.relays)?;
        Some(Self {
            relay_set_index,
            payout_tx_value: value.payout_tx_value.to_le_bytes(),
            subsidy: value.subsidy.to_le_bytes(),
        })
    }

    /// If it fails to find the relay set, returns None.
    /// relay_sets should be the same as the ones returned by the bidding service on initialize.
    #[allow(clippy::wrong_self_convention)]
    fn into_play_info(&self, relay_sets: &[RelaySet]) -> Option<PayoutInfo> {
        Some(PayoutInfo {
            relays: relay_sets.get(self.relay_set_index)?.clone(),
            payout_tx_value: U256::from_le_bytes(self.payout_tx_value),
            subsidy: I256::from_le_bytes(self.subsidy),
        })
    }
}

#[derive(Debug, Copy, Clone, ZeroCopySend)]
#[type_name("SlotBidderSealBidCommandRPC")]
#[repr(C)]
pub struct SlotBidderSealBidCommandRPC {
    pub session_id: u64,
    pub block_id: u64,
    pub seen_competition_bid: Option<[u8; U256_DATA_LENGTH]>,
    /// When this bid is a reaction so some event (eg: new block, new competition bid) we put here
    /// the creation time of that event so we can measure our reaction time.
    pub trigger_creation_time_us: Option<u64>,
    /// Count of valid payout infos.
    pub payout_infos_count: usize,
    /// Payout infos beyond payout_infos_count will be ignored.
    pub payout_infos: [PayoutInfoRPC; MAX_RELAY_SETS_COUNT],
}

impl SlotBidderSealBidCommandRPC {
    pub fn try_from(
        value: SlotBidderSealBidCommandWithSessionId,
        relay_sets: &[RelaySet],
    ) -> Option<Self> {
        let mut payout_infos = [PayoutInfoRPC::default(); MAX_RELAY_SETS_COUNT];
        let payout_infos_count = value.0.payout_info.len();
        if payout_infos_count > MAX_RELAY_SETS_COUNT {
            error!(
                payout_infos_count,
                MAX_RELAY_SETS_COUNT, "Too many payout infos"
            );
            return None;
        }
        for (index, payout_info_item) in value.0.payout_info.into_iter().enumerate() {
            payout_infos[index] = PayoutInfoRPC::try_from(payout_info_item, relay_sets)?;
        }
        Some(Self {
            session_id: value.1,
            block_id: value.0.block_id.0,
            seen_competition_bid: value.0.seen_competition_bid.map(|k| k.to_le_bytes()),
            trigger_creation_time_us: value
                .0
                .trigger_creation_time
                .map(offset_datetime_to_timestamp_us),
            payout_infos_count,
            payout_infos,
        })
    }

    pub fn into_slot_bidder_seal_bid_command(
        val: &SlotBidderSealBidCommandRPC,
        relay_sets: &[RelaySet],
    ) -> Option<SlotBidderSealBidCommand> {
        let mut payout_info = Vec::new();
        for index in 0..std::cmp::min(val.payout_infos_count, MAX_RELAY_SETS_COUNT) {
            payout_info.push(val.payout_infos[index].into_play_info(relay_sets)?);
        }
        Some(SlotBidderSealBidCommand {
            block_id: BuiltBlockId(val.block_id),
            seen_competition_bid: val.seen_competition_bid.map(|k| U256::from_le_bytes(k)),
            trigger_creation_time: val
                .trigger_creation_time_us
                .map(timestamp_us_to_offset_datetime),
            payout_info,
        })
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
