use std::sync::Arc;
use tokio::sync::broadcast;
use serde_json::json;
use alloy_primitives::{B256,U256};
use alloy_rpc_types_eth::state::{StateOverride, AccountOverride};
use std::collections::HashMap;
use uuid::Uuid;

use crate::{
    building::builders::{UnfinishedBlockBuildingSink, UnfinishedBlockBuildingSinkFactory},
    live_builder::payload_events::MevBoostSlotData,
    live_builder::streaming::block_subscription_server::start_block_subscription_server
};
use tracing::{error, info, warn};

use super::{
    bid_value_source::interfaces::{BidValueObs, BidValueSource},
    bidding::{
        interfaces::{BidMaker, BiddingService, SlotBidder},
        parallel_sealer_bid_maker::ParallelSealerBidMaker,
        sequential_sealer_bid_maker::SequentialSealerBidMaker,
        wallet_balance_watcher::WalletBalanceWatcher,
    },
    relay_submit::BuilderSinkFactory,
};

use serde_json::Value;

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
    /// See [ParallelSealerBidMaker]
    max_concurrent_seals: usize,
    /// State Diff WS Server
    state_diff_server: broadcast::Sender<Value>
}

impl BlockSealingBidderFactory {
    pub async fn new(
        bidding_service: Box<dyn BiddingService>,
        block_sink_factory: Box<dyn BuilderSinkFactory>,
        competition_bid_value_source: Arc<dyn BidValueSource + Send + Sync>,
        wallet_balance_watcher: WalletBalanceWatcher,
        max_concurrent_seals: usize,
    ) -> Self {
        let state_diff_server = start_block_subscription_server().await.expect("Failed to start block subscription server");
        Self {
            bidding_service,
            block_sink_factory,
            competition_bid_value_source,
            wallet_balance_watcher,
            max_concurrent_seals,
            state_diff_server
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
            .update_to_block(slot_data.block() - 1)
        {
            Ok(landed_blocks) => self
                .bidding_service
                .update_new_landed_blocks_detected(&landed_blocks),
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
        let sealer: Box<dyn BidMaker + Send + Sync> = if self.max_concurrent_seals == 1 {
            Box::new(SequentialSealerBidMaker::new(
                Arc::from(finished_block_sink),
                cancel.clone(),
            ))
        } else {
            Box::new(ParallelSealerBidMaker::new(
                self.max_concurrent_seals,
                Arc::from(finished_block_sink),
                cancel.clone(),
            ))
        };

        let slot_bidder: Arc<dyn SlotBidder> = self.bidding_service.create_slot_bidder(
            slot_data.block(),
            slot_data.slot(),
            slot_data.timestamp(),
            sealer,
            cancel.clone(),
        );

        let res = BlockSealingBidder::new(
            slot_data,
            slot_bidder,
            self.competition_bid_value_source.clone(),
            self.state_diff_server.clone()
        );

        Arc::new(res)
    }
}

/// Helper object containing the bidder.
/// It just forwards new blocks and new competitions bids (via SlotBidderToBidValueObs) to the bidder.
#[derive(Debug)]
struct BlockSealingBidder {
    /// Bidder we ask how to finish the blocks.
    bid_value_source_to_unsubscribe: Arc<dyn BidValueObs + Send + Sync>,
    /// Used to unsubscribe on drop.
    competition_bid_value_source: Arc<dyn BidValueSource + Send + Sync>,
    bidder: Arc<dyn SlotBidder>,
    state_diff_server: broadcast::Sender<Value>
}

impl BlockSealingBidder {
    pub fn new(
        slot_data: MevBoostSlotData,
        bidder: Arc<dyn SlotBidder>,
        competition_bid_value_source: Arc<dyn BidValueSource + Send + Sync>,
        state_diff_server: broadcast::Sender<Value>
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
            state_diff_server
        }
    }
}

impl UnfinishedBlockBuildingSink for BlockSealingBidder {
    fn new_block(
        &self,
        block: Box<dyn crate::building::builders::block_building_helper::BlockBuildingHelper>,
    ) {
        let building_context = block.building_context();
        let bundle_state = block.get_bundle_state().state();

        // Create a new StateOverride object to store the changes
        let mut pending_state = StateOverride::new();

        // Iterate through each address and account in the bundle state
        for (address, account) in bundle_state.iter() {
            let mut account_override = AccountOverride::default();

            let mut state_diff = HashMap::new();
            for (storage_key, storage_slot) in &account.storage {
                let key = B256::from(*storage_key);
                let value = B256::from(storage_slot.present_value);
                state_diff.insert(key, value);
            }

            if !state_diff.is_empty() {
                account_override.state_diff = Some(state_diff);
                pending_state.insert(*address, account_override);
            }

        }

        let block_data = json!({
            "blockNumber": building_context.block_env.number,
            "blockTimestamp": building_context.block_env.timestamp,
            "blockUuid": Uuid::new_v4(),
            "pendingState": pending_state
        });

        if let Err(_e) = self.state_diff_server.send(block_data) {
            warn!("Failed to send block data");
        }

        info!(
            "Block generated. Order Count: {}", block.built_block_trace().included_orders.len()
        );

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