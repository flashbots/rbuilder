use alloy_rpc_types_beacon::relay::SubmitBlockRequest as AlloySubmitBlockRequest;
use alloy_signer_local::PrivateKeySigner;
use rbuilder::{
    building::BuiltBlockTrace,
    live_builder::{
        block_output::bidding_service_interface::{BidObserver, RelaySet},
        payload_events::MevBoostSlotData,
    },
};
use redis::RedisError;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::{
    best_true_value_pusher::{
        Backend, BuiltBlockInfo, BuiltBlockInfoPusher, LastBuiltBlockInfoCell,
    },
    blocks_processor_backend::BlocksProcessorBackend,
    redis_backend::RedisBackend,
};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Unable to init redis connection : {0}")]
    Redis(#[from] RedisError),
    #[error("BlocksProcessor backend error: {0}")]
    BlocksProcessor(#[from] super::blocks_processor_backend::Error),
}

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug)]
pub struct BestTrueValueObserver {
    best_local_value: LastBuiltBlockInfoCell,
}

impl BestTrueValueObserver {
    /// Constructor using a redis channel backend
    pub fn new_redis(
        tbv_push_redis_url: String,
        tbv_push_redis_channel: String,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        let best_true_value_redis = redis::Client::open(tbv_push_redis_url)?;
        let redis_backend = RedisBackend::new(best_true_value_redis, tbv_push_redis_channel);
        Self::new(redis_backend, cancellation_token)
    }

    /// Constructor using signed JSON-RPC block-processor API
    pub fn new_block_processor(
        url: String,
        signer: PrivateKeySigner,
        max_concurrent_requests: usize,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        let backend = BlocksProcessorBackend::new(url, signer, max_concurrent_requests)?;
        Self::new(backend, cancellation_token)
    }

    fn new<BackendType: Backend + Send + 'static>(
        backend: BackendType,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        let last_local_value = LastBuiltBlockInfoCell::default();
        let pusher =
            BuiltBlockInfoPusher::new(last_local_value.clone(), backend, cancellation_token);
        std::thread::spawn(move || pusher.run_push_task());
        Ok(BestTrueValueObserver {
            best_local_value: last_local_value,
        })
    }
}

impl BidObserver for BestTrueValueObserver {
    fn block_submitted(
        &self,
        slot_data: &MevBoostSlotData,
        _submit_block_request: Arc<AlloySubmitBlockRequest>,
        built_block_trace: Arc<BuiltBlockTrace>,
        builder_name: String,
        _best_bid_value: alloy_primitives::U256,
        _relays: &RelaySet,
    ) {
        let block_info = BuiltBlockInfo::new(
            slot_data.block(),
            slot_data.slot(),
            built_block_trace.true_bid_value,
            built_block_trace.bid_value,
            built_block_trace.subsidy,
            builder_name,
            slot_data.timestamp().unix_timestamp() as u64,
        );
        self.best_local_value.update_value_safe(block_info);
    }
}
