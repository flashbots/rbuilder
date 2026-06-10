//! Block sink that finalizes blocks and hands them to an engine-API payload
//! job instead of submitting them to MEV-Boost relays.
//!
//! Used for chains where the proposer requests the payload from its own node
//! via `engine_forkchoiceUpdated`/`engine_getPayload` (Arc: Malachite BFT
//! consensus drives the engine API). There is no bidding: every block that
//! improves on the current best (by true block value) is finalized immediately
//! and published, and `engine_getPayload` returns the latest published block.
//!
//! The [`crate::live_builder::payload_events::MevBoostSlotData::payload_id`]
//! (`InternalPayloadId`, u64) must be the big-endian representation of the
//! engine API `PayloadId` so the bridge driving the engine API can find the
//! payload for a job (see [`EnginePayloadRegistry`]).

use super::{
    bidding_service_interface::CompetitionBidContext, UnfinishedBlockBuildingSink,
    UnfinishedBlockBuildingSinkFactory,
};
use crate::{
    building::{
        builders::block_building_helper::BiddableUnfinishedBlock, ThreadBlockBuildingContext,
    },
    live_builder::{
        building::built_block_cache::BuiltBlockCache,
        payload_events::{InternalPayloadId, MevBoostSlotData},
    },
};
use ahash::HashMap;
use alloy_eips::eip7685::Requests;
use alloy_primitives::{I256, U256};
use parking_lot::Mutex;
use reth::payload::PayloadId;
use reth_ethereum_engine_primitives::EthBuiltPayload;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace};

/// Glue between the engine-API payload jobs and the per-slot block sinks.
///
/// The engine bridge registers a cell when a payload job starts; the sink
/// publishes every new best finalized block into the cell; `engine_getPayload`
/// resolution reads (or awaits) the latest value.
#[derive(Debug, Default)]
pub struct EnginePayloadRegistry {
    cells: Mutex<HashMap<InternalPayloadId, watch::Sender<Option<EthBuiltPayload>>>>,
}

impl EnginePayloadRegistry {
    /// Registers a payload job. Returns the receiver the engine bridge awaits on.
    pub fn register(
        &self,
        payload_id: InternalPayloadId,
    ) -> watch::Receiver<Option<EthBuiltPayload>> {
        let (sender, receiver) = watch::channel(None);
        self.cells.lock().insert(payload_id, sender);
        receiver
    }

    /// Removes a finished payload job.
    pub fn unregister(&self, payload_id: InternalPayloadId) {
        self.cells.lock().remove(&payload_id);
    }

    fn publish(&self, payload_id: InternalPayloadId, payload: EthBuiltPayload) {
        if let Some(sender) = self.cells.lock().get(&payload_id) {
            sender.send_replace(Some(payload));
        } else {
            trace!(payload_id, "No engine payload cell for built block (job already resolved?)");
        }
    }
}

/// [`UnfinishedBlockBuildingSinkFactory`] for the engine-API flow.
#[derive(Debug)]
pub struct EnginePayloadSinkFactory {
    registry: Arc<EnginePayloadRegistry>,
}

impl EnginePayloadSinkFactory {
    pub fn new(registry: Arc<EnginePayloadRegistry>) -> Self {
        Self { registry }
    }
}

impl UnfinishedBlockBuildingSinkFactory for EnginePayloadSinkFactory {
    fn create_sink(
        &mut self,
        slot_data: MevBoostSlotData,
        _built_block_cache: Arc<BuiltBlockCache>,
        cancel: CancellationToken,
    ) -> Arc<dyn UnfinishedBlockBuildingSink> {
        Arc::new(EnginePayloadSink {
            registry: self.registry.clone(),
            internal_payload_id: slot_data.payload_id,
            payload_id: PayloadId::new(slot_data.payload_id.to_be_bytes()),
            state: Mutex::new(EnginePayloadSinkState {
                best_value: None,
                local_ctx: ThreadBlockBuildingContext::default(),
            }),
            cancel,
        })
    }
}

struct EnginePayloadSinkState {
    best_value: Option<U256>,
    local_ctx: ThreadBlockBuildingContext,
}

/// Per-slot sink: keeps the best block (by true block value), finalizes it and
/// publishes it to the registry cell.
struct EnginePayloadSink {
    registry: Arc<EnginePayloadRegistry>,
    internal_payload_id: InternalPayloadId,
    payload_id: PayloadId,
    state: Mutex<EnginePayloadSinkState>,
    cancel: CancellationToken,
}

impl std::fmt::Debug for EnginePayloadSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnginePayloadSink")
            .field("payload_id", &self.payload_id)
            .finish_non_exhaustive()
    }
}

impl UnfinishedBlockBuildingSink for EnginePayloadSink {
    fn new_block(&self, block: BiddableUnfinishedBlock) {
        if self.cancel.is_cancelled() {
            return;
        }
        // The lock serializes finalization between the building algorithm
        // threads; best_value is only updated after a successful finalize.
        let mut state = self.state.lock();
        let true_block_value = block.true_block_value;
        if state
            .best_value
            .is_some_and(|best| true_block_value <= best)
        {
            return;
        }
        let builder_name = block.block().builder_name().to_string();
        let mut helper = block.into_building_helper();
        let result = match helper.finalize_block(
            &mut state.local_ctx,
            true_block_value,
            I256::ZERO,
            CompetitionBidContext::no_competition_bid(),
        ) {
            Ok(result) => result,
            Err(err) => {
                if err.is_critical() {
                    error!(?err, "Failed to finalize block for engine payload");
                }
                return;
            }
        };
        state.best_value = Some(true_block_value);
        drop(state);

        let block = result.block;
        let payload: EthBuiltPayload = EthBuiltPayload::new(
            self.payload_id,
            Arc::new(block.sealed_block),
            true_block_value,
            Some(Requests::new(block.execution_requests)),
        );
        info!(
            payload_id = %self.payload_id,
            block = payload.block().number,
            builder_name,
            true_block_value = %true_block_value,
            "Publishing new best engine payload"
        );
        self.registry.publish(self.internal_payload_id, payload);
    }
}
