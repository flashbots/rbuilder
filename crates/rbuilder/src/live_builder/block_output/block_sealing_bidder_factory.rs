use std::sync::Arc;

use crate::{
    building::builders::{UnfinishedBlockBuildingSink, UnfinishedBlockBuildingSinkFactory},
    live_builder::payload_events::MevBoostSlotData,
};
use alloy_primitives::U256;
use tracing::error;

use super::{
    bid_value_source::interfaces::{BidValueObs, BidValueSource},
    bidding::{
        interfaces::{BiddingService, SlotBidder},
        sequential_sealer_bid_maker::SequentialSealerBidMaker,
        wallet_balance_watcher::WalletBalanceWatcher,
    },
    relay_submit::BuilderSinkFactory,
};

/// UnfinishedBlockBuildingSinkFactory to bid blocks against the competition.
/// Blocks are given to a SlotBidder (created per block).
/// SlotBidder bids using a SequentialSealerBidMaker (created per block).
/// SequentialSealerBidMaker sends the bids to a BlockBuildingSink (created per block).
/// SlotBidder is subscribed to the BidValueSource.
#[derive(Debug)]
pub struct BlockSealingBidderFactory {
    /// Factory for the SlotBidder for blocks.
    bidding_service: Box<dyn BiddingService>,
    /// Factory for the final destination for blocks.
    block_sink_factory: Box<dyn BuilderSinkFactory>,
    /// SlotBidder are subscribed to the proper block in the bid_value_source.
    competition_bid_value_source: Arc<dyn BidValueSource + Send + Sync>,
    wallet_balance_watcher: WalletBalanceWatcher,
}

impl BlockSealingBidderFactory {
    pub fn new(
        bidding_service: Box<dyn BiddingService>,
        block_sink_factory: Box<dyn BuilderSinkFactory>,
        competition_bid_value_source: Arc<dyn BidValueSource + Send + Sync>,
        wallet_balance_watcher: WalletBalanceWatcher,
    ) -> Self {
        Self {
            bidding_service,
            block_sink_factory,
            competition_bid_value_source,
            wallet_balance_watcher,
        }
    }
}

/// Struct to solve trait upcasting not supported in rust stable.
#[derive(Debug)]
struct SlotBidderToBidValueObs {
    bidder: Arc<dyn SlotBidder>,
}

impl BidValueObs for SlotBidderToBidValueObs {
    fn update_new_bid(&self, bid: U256) {
        self.bidder.update_new_bid(bid);
    }
}

impl UnfinishedBlockBuildingSinkFactory for BlockSealingBidderFactory {
    fn create_sink(
        &mut self,
        slot_data: MevBoostSlotData,
        cancel: tokio_util::sync::CancellationToken,
    ) -> std::sync::Arc<dyn crate::building::builders::UnfinishedBlockBuildingSink> {
        match self
            .wallet_balance_watcher
            .update_to_block(slot_data.block())
        {
            Ok(landed_blocks) => self
                .bidding_service
                .update_new_landed_blocks_detected(landed_blocks),
            Err(error) => {
                error!(error=?error, "Error updating wallet state");
                self.bidding_service
                    .update_failed_reading_new_landed_blocks()
            }
        }

        let finished_block_sink = self.block_sink_factory.create_builder_sink(
            slot_data.clone(),
            self.competition_bid_value_source.clone(),
            cancel.clone(),
        );
        let sealer = SequentialSealerBidMaker::new(Arc::from(finished_block_sink), cancel.clone());

        let slot_bidder: Arc<dyn SlotBidder> = self.bidding_service.create_slot_bidder(
            slot_data.block(),
            slot_data.slot(),
            slot_data.timestamp().unix_timestamp() as u64,
            Box::new(sealer),
        );

        let res = BlockSealingBidder::new(
            slot_data,
            slot_bidder,
            self.competition_bid_value_source.clone(),
        );

        Arc::new(res)
    }
}

#[derive(Debug)]
struct BlockSealingBidder {
    /// Bidder we ask how to finish the blocks.
    bid_value_source_to_unsubscribe: Arc<dyn BidValueObs + Send + Sync>,
    /// Used to unsubscribe on drop.
    competition_bid_value_source: Arc<dyn BidValueSource + Send + Sync>,
    bidder: Arc<dyn SlotBidder>,
}

impl BlockSealingBidder {
    pub fn new(
        slot_data: MevBoostSlotData,
        bidder: Arc<dyn SlotBidder>,
        competition_bid_value_source: Arc<dyn BidValueSource + Send + Sync>,
    ) -> Self {
        let slot_bidder_to_bid_value_obs: Arc<dyn BidValueObs + Send + Sync> =
            Arc::new(SlotBidderToBidValueObs {
                bidder: bidder.clone(),
            });

        competition_bid_value_source.subscribe(
            slot_data.block(),
            slot_data.slot(),
            slot_bidder_to_bid_value_obs.clone(),
        );

        Self {
            bid_value_source_to_unsubscribe: slot_bidder_to_bid_value_obs,
            competition_bid_value_source,
            bidder,
        }
    }
}

impl UnfinishedBlockBuildingSink for BlockSealingBidder {
    fn new_block(
        &self,
        block: Box<dyn crate::building::builders::block_building_helper::BlockBuildingHelper>,
    ) {
        self.bidder.new_block(block);
    }

    fn can_use_suggested_fee_recipient_as_coinbase(&self) -> bool {
        self.bidder.can_use_suggested_fee_recipient_as_coinbase()
    }
}

impl Drop for BlockSealingBidder {
    fn drop(&mut self) {
        self.competition_bid_value_source
            .unsubscribe(self.bid_value_source_to_unsubscribe.clone());
    }
}
