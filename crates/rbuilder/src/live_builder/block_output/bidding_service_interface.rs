use std::sync::Arc;

use alloy_primitives::{BlockHash, BlockNumber, I256, U256};
use alloy_rpc_types_beacon::relay::SubmitBlockRequest as AlloySubmitBlockRequest;
use bid_scraper::{
    bid_sender::{BidSender, BidSenderError},
    types::ScrapedRelayBlockBid,
};
use derivative::Derivative;
use mockall::automock;
use rbuilder_primitives::mev_boost::MevBoostRelayID;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::{
    building::{
        builders::{block_building_helper::BiddableUnfinishedBlock, BuiltBlockId},
        BuiltBlockTrace,
    },
    live_builder::payload_events::MevBoostSlotData,
    telemetry::inc_bids_received,
};

/// Trait that receives every bid made by us to the relays.
#[automock]
pub trait BidObserver: std::fmt::Debug {
    /// This should NOT block since it's executed in the submitting thread.
    fn block_submitted(
        &self,
        slot_data: &MevBoostSlotData,
        submit_block_request: Arc<AlloySubmitBlockRequest>,
        built_block_trace: Arc<BuiltBlockTrace>,
        builder_name: String,
        best_bid_value: U256,
        relays: &RelaySet,
    );
}

#[derive(Debug)]
pub struct NullBidObserver {}

impl BidObserver for NullBidObserver {
    fn block_submitted(
        &self,
        _slot_data: &MevBoostSlotData,
        _submit_block_request: Arc<AlloySubmitBlockRequest>,
        _built_block_trace: Arc<BuiltBlockTrace>,
        _builder_name: String,
        _best_bid_value: U256,
        _relays: &RelaySet,
    ) {
    }
}

/// Info about a onchain block from reth.
#[derive(Eq, PartialEq, Clone, Debug)]
pub struct LandedBlockInfo {
    pub block_number: BlockNumber,
    pub block_timestamp: OffsetDateTime,
    pub builder_balance: U256,
    /// true -> we landed this block.
    /// If false we could have landed it in coinbase == fee recipient mode but balance wouldn't change so we don't care.
    pub beneficiary_is_builder: bool,
}

/// Uniquely identifies the head of the chain we are bidding.
#[derive(Eq, PartialEq, Clone, Debug, Hash)]
pub struct SlotBlockId {
    pub slot: u64,
    pub block: u64,
    pub parent_block_hash: BlockHash,
}

impl SlotBlockId {
    /// Creates a new SlotBlockId instance.
    pub fn new(slot: u64, block: u64, parent_block_hash: BlockHash) -> Self {
        Self {
            slot,
            block,
            parent_block_hash,
        }
    }
}

/// Selected information coming from a BlockBuildingHelper.
#[derive(Derivative, Clone, Debug)]
#[derivative(PartialEq, Eq)]
pub struct BuiltBlockDescriptorForSlotBidder {
    pub true_block_value: U256,
    pub id: BuiltBlockId,
    /// For metrics
    #[derivative(PartialEq = "ignore")]
    pub creation_time: OffsetDateTime,
}

impl BuiltBlockDescriptorForSlotBidder {
    pub fn new(id: BuiltBlockId, unfinished_block: &BiddableUnfinishedBlock) -> Self {
        Self {
            true_block_value: unfinished_block.true_block_value,
            id,
            creation_time: OffsetDateTime::now_utc(),
        }
    }
}

/// A set of relays that get the same bid.
#[derive(Clone, Eq, PartialEq, Debug, Hash)]
pub struct RelaySet {
    /// sorted alphabetically to make eq work
    relays: Vec<MevBoostRelayID>,
}

impl RelaySet {
    pub fn new(mut relays: Vec<MevBoostRelayID>) -> Self {
        relays.sort();
        Self { relays }
    }

    pub fn relays(&self) -> &[MevBoostRelayID] {
        &self.relays
    }
}

impl From<Vec<MevBoostRelayID>> for RelaySet {
    fn from(relays: Vec<MevBoostRelayID>) -> Self {
        Self::new(relays)
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct PayoutInfo {
    /// Relays that should get this bid.
    pub relays: RelaySet,
    pub payout_tx_value: U256,
    /// Subsidy used in the bid.
    pub subsidy: I256,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct SlotBidderSealBidCommand {
    pub block_id: BuiltBlockId,
    pub seen_competition_bid: Option<U256>,
    /// When this bid is a reaction so some event (eg: new block, new competition bid) we put here
    /// the creation time of that event so we can measure our reaction time.
    pub trigger_creation_time: Option<OffsetDateTime>,
    /// All the different bids to be made.
    pub payout_info: Vec<PayoutInfo>,
}

#[automock]
pub trait BlockSealInterfaceForSlotBidder {
    fn seal_bid(&self, bid: SlotBidderSealBidCommand);
}

// /// BlockBid + extra info needed to measure bis travel times on the bidding service.
#[derive(Derivative, Clone, Debug)]
#[derivative(PartialEq, Eq)]
pub struct ScrapedRelayBlockBidWithStats {
    pub bid: ScrapedRelayBlockBid,
    /// Time this strucut was created, just before sending it to the bidding service
    #[derivative(PartialEq = "ignore")]
    pub creation_time: OffsetDateTime,
}

impl ScrapedRelayBlockBidWithStats {
    pub fn new(bid: ScrapedRelayBlockBid) -> Self {
        Self {
            bid,
            creation_time: OffsetDateTime::now_utc(),
        }
    }

    pub fn new_for_deserialization(
        bid: ScrapedRelayBlockBid,
        creation_time: OffsetDateTime,
    ) -> Self {
        Self { bid, creation_time }
    }
}

#[automock]
pub trait SlotBidder: Send + Sync {
    fn notify_new_built_block(&self, block_descriptor: BuiltBlockDescriptorForSlotBidder);
}

pub trait BiddingService: Send + Sync {
    fn create_slot_bidder(
        &self,
        slot_block_id: SlotBlockId,
        slot_timestamp: OffsetDateTime,
        block_seal_handle: Box<dyn BlockSealInterfaceForSlotBidder + Send + Sync>,
        cancel: CancellationToken,
    ) -> Arc<dyn SlotBidder>;
    /// set of all RelaySet that will be used to bid.
    /// Not &[RelaySet] because it caused problems with some Mutex<BiddingService>.
    fn relay_sets(&self) -> Vec<RelaySet>;

    fn observe_relay_bids(&self, bid: ScrapedRelayBlockBidWithStats);

    fn update_new_landed_blocks_detected(&self, landed_blocks: &[LandedBlockInfo]);

    fn update_failed_reading_new_landed_blocks(&self);
}

pub struct BiddingService2BidSender {
    inner: Arc<dyn BiddingService>,
}

impl BiddingService2BidSender {
    pub fn new(inner: Arc<dyn BiddingService>) -> Self {
        Self { inner }
    }
}

impl BidSender for BiddingService2BidSender {
    fn send(&self, bid: ScrapedRelayBlockBid) -> Result<(), BidSenderError> {
        inc_bids_received(&bid);
        self.inner
            .observe_relay_bids(ScrapedRelayBlockBidWithStats::new(bid));
        Ok(())
    }
}
