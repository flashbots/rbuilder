use serde::Deserialize;

/// config for the p2p epbs builder service.
#[derive(Debug, Clone)]
pub struct EpbsP2PConfig {
    /// Whether P2P bid broadcasting is enabled.
    pub enabled: bool,
    /// Milliseconds into slot to start bidding (default: 0 = slot start).
    pub bid_start_ms: u64,
    /// Milliseconds into slot to stop bidding (default: 4000).
    pub bid_end_ms: u64,
    /// Interval between bid resubmissions in ms. 0 = single bid mode.
    pub bid_interval_ms: u64,
    /// Value increment per resubmission in gwei.
    pub bid_value_increment_gwei: u64,
    /// Genesis time from the beacon chain (seconds since unix epoch).
    pub genesis_time: u64,
    /// Slot duration in seconds (from beacon spec).
    pub seconds_per_slot: u64,
}

impl Default for EpbsP2PConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bid_start_ms: 0,
            bid_end_ms: 4000,
            bid_interval_ms: 500,
            bid_value_increment_gwei: 0,
            genesis_time: 0,
            seconds_per_slot: 12,
        }
    }
}

///  event types used by the P2P service event loop.
#[derive(Debug, Clone)]
pub enum P2PEvent {
    /// A new head was seen on the beacon chain.
    NewHead(HeadEventData),
    /// A competing bid was seen on the P2P network.
    BidReceived(rbuilder_primitives::epbs::SignedExecutionPayloadBid),
    /// Proposer preferences received from P2P gossip.
    ProposerPreferences(rbuilder_primitives::epbs::SignedProposerPreferences),
}

/// Extracted head event data relevant to the p2p builder.
#[derive(Debug, Clone, Deserialize)]
pub struct HeadEventData {
    pub slot: u64,
    pub block_root: alloy_primitives::B256,
    pub state_root: alloy_primitives::B256,
}
