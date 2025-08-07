use crate::{
    building::builders::UnfinishedBlockBuildingSinkFactory,
    live_builder::{
        block_output::bidding::interfaces::SlotBlockId, payload_events::MevBoostSlotData,
    },
    provider::StateProviderFactory,
};
use std::{fmt::Debug, sync::Arc};
use tracing::error;

use super::{
    bidding::{
        interfaces::BiddingService, sequential_sealer_bid_maker::SequentialSealerBidMaker,
        wallet_balance_watcher::WalletBalanceWatcher,
    },
    relay_submit::BuilderSinkFactory,
};

/// UnfinishedBlockBuildingSinkFactory to bid blocks against the competition.
/// Blocks are given to a slot bidder (UnfinishedBlockBuildingSink created per block by the BiddingService).
/// Slot bidder bids using a SequentialSealerBidMaker (created per block).
/// SequentialSealerBidMaker sends the bids to a BlockBuildingSink (created per block).
pub struct BlockSealingBidderFactory<P> {
    /// Factory for the SlotBidder for blocks.
    bidding_service: Arc<dyn BiddingService>,
    /// Factory for the final destination for blocks.
    block_sink_factory: Box<dyn BuilderSinkFactory>,
    wallet_balance_watcher: WalletBalanceWatcher<P>,
}

impl<P> Debug for BlockSealingBidderFactory<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockSealingBidderFactory")
            .field("bidding_service", &"Arc<dyn BiddingService>")
            .field("block_sink_factory", &"Box<dyn BuilderSinkFactory>")
            .finish()
    }
}

impl<P> BlockSealingBidderFactory<P> {
    pub fn new(
        bidding_service: Arc<dyn BiddingService>,
        block_sink_factory: Box<dyn BuilderSinkFactory>,
        wallet_balance_watcher: WalletBalanceWatcher<P>,
    ) -> Self {
        Self {
            bidding_service,
            block_sink_factory,
            wallet_balance_watcher,
        }
    }
}

impl<P> UnfinishedBlockBuildingSinkFactory for BlockSealingBidderFactory<P>
where
    P: StateProviderFactory,
{
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

        let finished_block_sink = self
            .block_sink_factory
            .create_builder_sink(slot_data.clone(), cancel.clone());
        let sealer = Box::new(SequentialSealerBidMaker::new(
            Arc::from(finished_block_sink),
            cancel.clone(),
        ));

        self.bidding_service.create_slot_bidder(
            SlotBlockId::new(
                slot_data.slot(),
                slot_data.block(),
                slot_data.parent_block_hash(),
            ),
            slot_data.timestamp(),
            sealer,
            cancel.clone(),
        )
    }
}
