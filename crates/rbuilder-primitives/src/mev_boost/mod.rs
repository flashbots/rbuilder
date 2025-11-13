use crate::OrderId;
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_beacon::BlsPublicKey;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use std::time::Duration;

mod submit_block;
pub use submit_block::*;

mod submit_header;
pub use submit_header::*;

pub mod ssz_roots;

mod optimistic_v3;
pub use optimistic_v3::*;

mod adjustment;
pub use adjustment::*;

/// Usually human readable id for relays. Not used on anything on any protocol just to identify the relays.
pub type MevBoostRelayID = String;

/// Timeout for requesting current epoch data from the MEV-Boost relay.
pub const MEV_BOOST_SLOT_INFO_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Modes for a relay since we may use them for different purposes.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq, Default)]
pub enum RelayMode {
    /// Submits bids, gets slot info. No extra headers on bidding.
    #[serde(rename = "full")]
    #[default]
    Full,
    /// Only gets slot info.
    #[serde(rename = "slot_info")]
    GetSlotInfoOnly,
    /// Submits bids with extra headers. Is not used to get slot info.
    #[serde(rename = "test")]
    Test,
}

impl RelayMode {
    pub fn submits_bids(&self) -> bool {
        match self {
            RelayMode::Full => true,
            RelayMode::GetSlotInfoOnly => false,
            RelayMode::Test => true,
        }
    }
    pub fn gets_slot_info(&self) -> bool {
        match self {
            RelayMode::Full => true,
            RelayMode::GetSlotInfoOnly => true,
            RelayMode::Test => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum KnownRelay {
    Flashbots,
    BloxrouteMaxProfit,
    BloxrouteEthical,
    BloxrouteRegulated,
    Eden,
    SecureRpc,
    Ultrasound,
    Agnostic,
    Aestus,
    Wenmerge,
    Titan,
}

pub const RELAYS: [KnownRelay; 10] = [
    KnownRelay::Flashbots,
    KnownRelay::BloxrouteMaxProfit,
    KnownRelay::BloxrouteRegulated,
    KnownRelay::Eden,
    KnownRelay::SecureRpc,
    KnownRelay::Ultrasound,
    KnownRelay::Agnostic,
    KnownRelay::Aestus,
    KnownRelay::Wenmerge,
    KnownRelay::Titan,
];

impl KnownRelay {
    pub fn url(&self) -> Url {
        Url::parse(match self {
            KnownRelay::Flashbots => "https://0xac6e77dfe25ecd6110b8e780608cce0dab71fdd5ebea22a16c0205200f2f8e2e3ad3b71d3499c54ad14d6c21b41a37ae@boost-relay.flashbots.net",
            KnownRelay::BloxrouteMaxProfit => "https://0x8b5d2e73e2a3a55c6c87b8b6eb92e0149a125c852751db1422fa951e42a09b82c142c3ea98d0d9930b056a3bc9896b8f@bloxroute.max-profit.blxrbdn.com",
            KnownRelay::BloxrouteEthical => "https://0xad0a8bb54565c2211cee576363f3a347089d2f07cf72679d16911d740262694cadb62d7fd7483f27afd714ca0f1b9118@bloxroute.ethical.blxrbdn.com",
            KnownRelay::BloxrouteRegulated => "https://0xb0b07cd0abef743db4260b0ed50619cf6ad4d82064cb4fbec9d3ec530f7c5e6793d9f286c4e082c0244ffb9f2658fe88@bloxroute.regulated.blxrbdn.com",
            KnownRelay::Eden => "https://0xb3ee7afcf27f1f1259ac1787876318c6584ee353097a50ed84f51a1f21a323b3736f271a895c7ce918c038e4265918be@relay.edennetwork.io",
            KnownRelay::SecureRpc => "https://0x98650451ba02064f7b000f5768cf0cf4d4e492317d82871bdc87ef841a0743f69f0f1eea11168503240ac35d101c9135@mainnet-relay.securerpc.com",
            KnownRelay::Ultrasound => "https://0xa1559ace749633b997cb3fdacffb890aeebdb0f5a3b6aaa7eeeaf1a38af0a8fe88b9e4b1f61f236d2e64d95733327a62@relay.ultrasound.money",
            KnownRelay::Agnostic => "https://0xa7ab7a996c8584251c8f925da3170bdfd6ebc75d50f5ddc4050a6fdc77f2a3b5fce2cc750d0865e05d7228af97d69561@agnostic-relay.net",
            KnownRelay::Aestus => "https://0xa15b52576bcbf1072f4a011c0f99f9fb6c66f3e1ff321f11f461d15e31b1cb359caa092c71bbded0bae5b5ea401aab7e@aestus.live",
            KnownRelay::Wenmerge => "https://0x8c7d33605ecef85403f8b7289c8058f440cbb6bf72b055dfe2f3e2c6695b6a1ea5a9cd0eb3a7982927a463feb4c3dae2@relay.wenmerge.com",
            KnownRelay::Titan => "https://0x8c4ed5e24fe5c6ae21018437bde147693f68cda427cd1122cf20819c30eda7ed74f72dece09bb313f2a1855595ab677d@titanrelay.xyz",
        }).unwrap()
    }

    pub fn name(&self) -> String {
        match self {
            KnownRelay::Flashbots => "flashbots",
            KnownRelay::BloxrouteMaxProfit => "bloxroute_max_profit",
            KnownRelay::BloxrouteEthical => "bloxroute_ethical",
            KnownRelay::BloxrouteRegulated => "bloxroute_regulated",
            KnownRelay::Eden => "eden",
            KnownRelay::SecureRpc => "secure_rpc",
            KnownRelay::Ultrasound => "ultrasound",
            KnownRelay::Agnostic => "agnostic",
            KnownRelay::Aestus => "aestus",
            KnownRelay::Wenmerge => "wenmerge",
            KnownRelay::Titan => "titan",
        }
        .to_string()
    }

    pub fn is_bloxroute(&self) -> bool {
        matches!(
            self,
            Self::BloxrouteMaxProfit | Self::BloxrouteEthical | Self::BloxrouteRegulated
        )
    }
}

impl std::str::FromStr for KnownRelay {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "flashbots" => Ok(KnownRelay::Flashbots),
            "bloxroute_max_profit" => Ok(KnownRelay::BloxrouteMaxProfit),
            "bloxroute_ethical" => Ok(KnownRelay::BloxrouteEthical),
            "bloxroute_regulated" => Ok(KnownRelay::BloxrouteRegulated),
            "eden" => Ok(KnownRelay::Eden),
            "secure_rpc" => Ok(KnownRelay::SecureRpc),
            "ultrasound" => Ok(KnownRelay::Ultrasound),
            "agnostic" => Ok(KnownRelay::Agnostic),
            "aestus" => Ok(KnownRelay::Aestus),
            "wenmerge" => Ok(KnownRelay::Wenmerge),
            "titan" => Ok(KnownRelay::Titan),
            _ => Err(()),
        }
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Hash)]
pub struct ValidatorRegistrationMessage {
    pub fee_recipient: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub gas_limit: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub timestamp: u64,
    pub pubkey: BlsPublicKey,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Hash)]
pub struct ValidatorRegistration {
    pub message: ValidatorRegistrationMessage,
    pub signature: Bytes,
}

/// Info about a registered validator selected as proposer for a slot.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Hash)]
pub struct ValidatorSlotData {
    /// The slot number for the validator entry.
    #[serde_as(as = "DisplayFromStr")]
    pub slot: u64,
    /// The index of the validator.
    #[serde_as(as = "DisplayFromStr")]
    pub validator_index: u64,
    /// Details of the validator registration.
    pub entry: ValidatorRegistration,
    /// (Bloxroute) Collection of regional endpoints validator is connected to.
    #[serde(default)]
    pub regional_endpoints: Vec<BloxrouteRegionalEndpoint>,
}

/// Bloxroute validator RProxy details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct BloxrouteRegionalEndpoint {
    /// RProxy name
    pub name: String,
    /// RProxy region. Format: `city,region`.
    pub region: String,
    /// RProxy HTTP endpoint.
    pub http_endpoint: String,
    /// RProxy gRPC endpoint.
    pub grpc_endpoint: String,
    /// RProxy WS endpoint.
    pub websocket_endpoint: String,
}

#[derive(Clone, Debug)]
pub struct BidMetadata {
    pub sequence: u64,
    pub value: BidValueMetadata,
    pub order_ids: Vec<OrderId>,
    pub bundle_hashes: Vec<B256>,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct BidValueMetadata {
    pub coinbase_reward: U256,
    pub top_competitor_bid: Option<U256>,
}

#[derive(Clone, Debug)]
pub struct SubmitBlockRequestWithMetadata {
    pub submission: SubmitBlockRequest,
    pub metadata: BidMetadata,
}
