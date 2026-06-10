pub mod best_block_from_algorithms;
pub mod engine_payload_sink;
pub mod bidding_service_interface;
pub mod relay_submit;
pub mod true_value_bidding_service;
pub mod unfinished_block_processing;

use crate::{
    building::builders::block_building_helper::BiddableUnfinishedBlock,
    live_builder::{building::built_block_cache::BuiltBlockCache, payload_events::MevBoostSlotData},
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Per-slot destination for the (unfinished) blocks produced by the building
/// algorithms. The default implementation is the MEV-Boost pipeline
/// ([`unfinished_block_processing::UnfinishedBuiltBlocksInput`]); for
/// engine-API driven chains (Arc) blocks are finalized and handed to the
/// payload job instead.
pub trait UnfinishedBlockBuildingSink: std::fmt::Debug + Send + Sync {
    fn new_block(&self, block: BiddableUnfinishedBlock);
}

/// Creates an [`UnfinishedBlockBuildingSink`] for each building slot.
pub trait UnfinishedBlockBuildingSinkFactory: std::fmt::Debug + Send + Sync {
    fn create_sink(
        &mut self,
        slot_data: MevBoostSlotData,
        built_block_cache: Arc<BuiltBlockCache>,
        cancel: CancellationToken,
    ) -> Arc<dyn UnfinishedBlockBuildingSink>;
}
