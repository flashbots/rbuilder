use derivative::Derivative;
use ethers::types::{Address, H256, U256, U64};
use serde::{Deserialize, Serialize};

/// Id for each type of scraping method.
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub enum PublisherType {
    /// BidsPublisherService
    #[serde(rename = "bids")]
    RelayBids,
    /// HeadersPublisherService
    #[serde(rename = "headers")]
    RelayHeaders,
    #[serde(rename = "ultrasound_ws")]
    UltrasoundWs,
    #[serde(rename = "bloxroute_ws")]
    BloxrouteWs,
}

impl PublisherType {
    /// true: The source will publish only the current winning bid
    /// false: he source will publish every bid it receives.
    pub fn publishes_only_top_bid(&self) -> bool {
        match self {
            PublisherType::RelayBids => false,
            PublisherType::RelayHeaders => true,
            PublisherType::UltrasoundWs => true,
            PublisherType::BloxrouteWs => false,
        }
    }
}

/// Represents a single block bid scraped from the relay.
///
/// Trait implementations:
/// `PartialEq` - we voluntarily omit `seen_time` as it is metadata we add
/// `Hash` - we voluntarily omit `seen_time` (metadata that we add) and `relay_time` (not hashable and we don't care about it)
#[derive(Debug, Clone, Derivative, Serialize)]
#[derivative(Hash, PartialEq, Eq)]
pub struct BlockBid {
    // time when the bids-publisher saw & sent it.
    #[derivative(PartialEq = "ignore")]
    #[derivative(Hash = "ignore")]
    pub seen_time: f64,
    /// Specific instance id a the publisher_type running. Eg: we can have "ultrasound-us" and "ultrasound-eu" both of type PublisherType::UltrasoundWs
    pub publisher_name: String,
    pub publisher_type: PublisherType,
    // time that the relay gives us, from when it received the bid.
    #[derivative(Hash = "ignore")]
    pub relay_time: Option<f64>,

    /// Source of the bid (a single publisher can query multiple relays)
    pub relay_name: String,

    pub block_hash: H256,
    pub parent_hash: H256,
    pub value: U256,

    pub slot_number: U64,
    pub block_number: Option<U64>,

    pub builder_pubkey: Option<String>,
    pub extra_data: Option<String>,
    pub fee_recipient: Option<Address>,          // block COINBASE
    pub proposer_fee_recipient: Option<Address>, // validator address

    pub gas_used: Option<U64>,
    pub optimistic_submission: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::hash::{DefaultHasher, Hash, Hasher};

    impl Arbitrary for PublisherType {
        type Parameters = ();
        type Strategy = proptest::strategy::BoxedStrategy<PublisherType>;

        fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
            prop_oneof![
                Just(PublisherType::RelayBids),
                Just(PublisherType::RelayHeaders),
                Just(PublisherType::UltrasoundWs),
                Just(PublisherType::BloxrouteWs),
            ]
            .boxed()
        }
    }
    // TODO: derive `Arbitrary` instead
    impl Arbitrary for BlockBid {
        type Parameters = ();
        type Strategy = proptest::strategy::BoxedStrategy<BlockBid>;
        fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
            any::<(
                (
                    f64,
                    String,
                    PublisherType,
                    Option<f64>,
                    String,
                    [u8; 32],
                    [u8; 32],
                    [u8; 32],
                ),
                u64,
                Option<u64>,
                Option<String>,
                Option<String>,
                Option<[u8; 20]>,
                Option<[u8; 20]>,
                Option<u64>,
                Option<bool>,
            )>()
            .prop_map(
                |(
                    (
                        seen_time,
                        publisher_name,
                        publisher_type,
                        relay_time,
                        relay_name,
                        block_hash,
                        parent_hash,
                        value,
                    ),
                    slot_number,
                    block_number,
                    builder_pubkey,
                    extra_data,
                    fee_recipient,
                    proposer_fee_recipient,
                    gas_used,
                    optimistic_submission,
                )| {
                    BlockBid {
                        seen_time,
                        publisher_name,
                        publisher_type,
                        relay_time,
                        relay_name,
                        block_hash: H256::from_slice(&block_hash),
                        parent_hash: H256::from_slice(&parent_hash),
                        value: U256::from(&value),
                        slot_number: U64::from(slot_number),
                        block_number: block_number.map(U64::from),
                        builder_pubkey,
                        extra_data,
                        fee_recipient: fee_recipient.map(Address::from),
                        proposer_fee_recipient: proposer_fee_recipient.map(Address::from),
                        gas_used: gas_used.map(U64::from),
                        optimistic_submission,
                    }
                },
            )
            .boxed()
        }
    }

    proptest! {
        #[test]
        fn bid_equality((bid, other_bid, other_seen_time) in any::<(BlockBid, BlockBid, f64)>()) {
            let mut equivalent_bid = bid.clone();
            equivalent_bid.seen_time = other_seen_time;
            prop_assert_eq!(&bid, &equivalent_bid);

            prop_assert_ne!(bid, other_bid);
        }

        #[test]
        fn bid_hashing((bid, other_bid, other_seen_time, other_relay_time) in any::<(BlockBid, BlockBid, f64, Option<f64>)>()) {
            let mut equivalent_bid = bid.clone();
            equivalent_bid.seen_time = other_seen_time;
            equivalent_bid.relay_time = other_relay_time;

            let mut hasher1 = DefaultHasher::default();
            bid.hash(&mut hasher1);
            let bid_hash = hasher1.finish();

            let mut hasher2 = DefaultHasher::default();
            equivalent_bid.hash(&mut hasher2);

            prop_assert_eq!(bid_hash, hasher2.finish());

            let mut hasher3 = DefaultHasher::default();
            other_bid.hash(&mut hasher3);
            prop_assert_ne!(bid_hash, hasher3.finish());
        }
    }
}
