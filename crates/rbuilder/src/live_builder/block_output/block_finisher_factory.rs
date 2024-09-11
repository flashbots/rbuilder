use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tokio::sync::broadcast;

use crate::{
    building::builders::{UnfinishedBlockBuildingSink, UnfinishedBlockBuildingSinkFactory},
    live_builder::payload_events::MevBoostSlotData,
    live_builder::streaming::block_subscription_server::start_block_subscription_server,
};

use super::{
    bidding::{BiddingService, SlotBidder},
    block_finisher::BlockFinisher,
    relay_submit::{BuilderSinkFactory, NullBestBidSource},
};

#[derive(Debug)]
pub struct BlockFinisherFactory {
    bidding_service: Box<dyn BiddingService>,
    /// Factory for the final destination for blocks.
    block_sink_factory: Box<dyn BuilderSinkFactory>,
    tx: broadcast::Sender<String>
}

impl BlockFinisherFactory {
    pub async fn new(
        bidding_service: Box<dyn BiddingService>,
        block_sink_factory: Box<dyn BuilderSinkFactory>,
    ) -> eyre::Result<Self> {
        let tx = start_block_subscription_server().await?;
        Ok(Self {
            bidding_service,
            block_sink_factory,
            tx,
        })
    }
}

impl UnfinishedBlockBuildingSinkFactory for BlockFinisherFactory {
    fn create_sink(
        &mut self,
        slot_data: MevBoostSlotData,
        cancel: CancellationToken,
    ) -> Arc<dyn UnfinishedBlockBuildingSink> {
        let slot_bidder: Arc<dyn SlotBidder> = self.bidding_service.create_slot_bidder(
            slot_data.block(),
            slot_data.slot(),
            slot_data.timestamp().unix_timestamp() as u64,
        );
        let finished_block_sink = self.block_sink_factory.create_builder_sink(
            slot_data,
            Arc::new(NullBestBidSource {}),
            cancel.clone(),
        );

        let res = BlockFinisher::new(slot_bidder, Arc::from(finished_block_sink), self.tx.clone());
        Arc::new(res)
    }
}
