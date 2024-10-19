use std::sync::Arc;
use tokio::sync::broadcast;
use std::collections::HashMap;
use uuid::Uuid;
use alloy_primitives::U256;

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
use serde::Serialize;

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
    state_diff_server: broadcast::Sender<Value>,
    slot_timestamp: time::OffsetDateTime
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
            slot_timestamp: slot_data.timestamp(),
            state_diff_server
        }
    }
}

use alloy_primitives::{Address, B256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BlockStateUpdate {
    block_number: U256,
    block_timestamp: U256,
    block_uuid: Uuid,
    state_diff: HashMap<Address, AccountStateUpdate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]

struct AccountStateUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    storage_diff: Option<HashMap<B256, B256>>,
}

const STATE_STREAMING_START_DELTA: time::Duration = time::Duration::milliseconds(-2000);    

impl UnfinishedBlockBuildingSink for BlockSealingBidder {
    fn new_block(
        &self,
        block: Box<dyn crate::building::builders::block_building_helper::BlockBuildingHelper>,
    ) {
        let now = time::OffsetDateTime::now_utc();
        let streaming_start_time = self.slot_timestamp + STATE_STREAMING_START_DELTA;

        // Print the delta between now and slot timestamp
        let delta = now - self.slot_timestamp;
        info!("Seconds into slot: {}", delta.as_seconds_f64());
        
        if now >= streaming_start_time {
            let building_context = block.building_context();
            let bundle_state = block.get_bundle_state();

            let block_state_update = BlockStateUpdate {
                block_number: building_context.block_env.number.into(),
                block_timestamp: building_context.block_env.timestamp.into(),
                block_uuid: Uuid::new_v4(),
                state_diff: bundle_state.state.iter()
                    .filter_map(|(address, account)| {
                        // Skip accounts with empty code hash (EOAs)
                        if account.info.as_ref().map_or(true, |info| info.is_empty_code_hash()) {
                            return None;
                        }
                        // Populate storage_diff
                        Some((*address, AccountStateUpdate {
                            storage_diff: Some(account.storage.iter()
                                .map(|(slot, storage_slot)| (
                                    B256::from(*slot),
                                    B256::from(storage_slot.present_value)
                                ))
                                .collect()),
                        }))
                    })
                    .collect(),
            };

            // Serialize and send the block_state_update
            match serde_json::to_value(&block_state_update) {
                Ok(json_data) => {
                    if let Err(_e) = self.state_diff_server.send(json_data) {
                        warn!("Failed to send block data");
                    } else {
                        info!(
                            "Sent BlockStateUpdate: uuid={}",
                            block_state_update.block_uuid
                        );
                    }
                },
                Err(e) => error!("Failed to serialize block state diff update: {:?}", e),
            }
        }
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
