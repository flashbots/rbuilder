//! Clickhouse integration to save all the blocks we build and submit to relays.

use std::time::Duration;

use alloy_primitives::{utils::format_ether, U256};
use clickhouse::{Client, Row};
use rbuilder::{
    building::BuiltBlockTrace,
    live_builder::{
        block_output::bidding_service_interface::BidObserver, payload_events::MevBoostSlotData,
    },
};
use rbuilder_primitives::mev_boost::SubmitBlockRequest;
use rbuilder_utils::clickhouse::{
    backup::{
        metrics::NullMetrics,
        primitives::{ClickhouseIndexableData, ClickhouseRowExt},
    },
    serde::{option_u256, vec_u256},
    spawn_clickhouse_inserter_and_backup,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::error;

use crate::flashbots_config::BuiltBlocksClickhouseConfig;

/// BlockRow to insert in clickhouse and also as entry type for the indexer since the BlockRow is made from a few &objects so it makes no sense to have a Block type and copy all the fields.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct BlockRow {
    pub block_number: u64,
    pub profit: String,
    pub slot: u64,
    pub hash: String,
    pub gas_limit: u64,
    pub gas_used: u64,
    pub base_fee: u64,
    pub parent_hash: String,
    pub proposer_pubkey: String,
    pub proposer_fee_recipient: String,
    pub builder_pubkey: String,
    pub timestamp: u64,
    pub timestamp_datetime: i64,
    pub orders_closed_at: i64,
    pub sealed_at: i64,
    pub algorithm: String,

    #[serde(with = "option_u256")]
    pub true_value: Option<U256>,
    #[serde(with = "option_u256")]
    pub best_relay_value: Option<U256>,
    #[serde(with = "option_u256")]
    pub block_value: Option<U256>,

    pub used_bundle_hashes: Vec<String>,
    pub used_bundle_uuids: Vec<String>,
    pub used_sbundles_hashes: Vec<String>,
    pub delayed_payment_sources: Vec<String>,

    #[serde(with = "vec_u256")]
    pub delayed_payment_values: Vec<U256>,

    pub delayed_payment_addresses: Vec<String>,
}

impl ClickhouseRowExt for BlockRow {
    type TraceId = String;
    const TABLE_NAME: &'static str = "blocks";

    fn trace_id(&self) -> String {
        self.hash.clone()
    }

    fn to_row_ref(row: &Self) -> &<Self as Row>::Value<'_> {
        row
    }
}

impl ClickhouseIndexableData for BlockRow {
    type ClickhouseRowType = BlockRow;

    const DATA_NAME: &'static str = <BlockRow as ClickhouseRowExt>::TABLE_NAME;

    fn trace_id(&self) -> String {
        self.hash.clone()
    }

    fn to_row(self, _builder_name: String) -> Self::ClickhouseRowType {
        self
    }
}

const KILO: u64 = 1024;
const MEGA: u64 = KILO * KILO;

// Super worst scenario we submit 500 blocks per second so we have 2 seconds of buffer.
const BUILT_BLOCKS_CHANNEL_SIZE: usize = 1024;
const BLOCKS_TABLE_NAME: &str = "blocks";
const DEFAULT_MAX_DISK_SIZE_MB: u64 = 10 * KILO;
const DEFAULT_MAX_MEMORY_SIZE_MB: u64 = 1 * KILO;
#[derive(Debug)]
pub struct BuiltBlocksWriter {
    blocks_tx: mpsc::Sender<BlockRow>,
}

impl BuiltBlocksWriter {
    pub fn new(config: BuiltBlocksClickhouseConfig, cancellation_token: CancellationToken) -> Self {
        let client = Client::default()
            .with_url(config.host)
            .with_database(config.database)
            .with_user(config.username)
            .with_password(config.password)
            .with_validation(false); // CRITICAL for U256 serialization.

        let task_manager = rbuilder_utils::tasks::TaskManager::current();
        let task_executor = task_manager.executor();

        let (block_tx, block_rx) = mpsc::channel::<BlockRow>(BUILT_BLOCKS_CHANNEL_SIZE);
        spawn_clickhouse_inserter_and_backup::<BlockRow, BlockRow, NullMetrics>(
            &client,
            block_rx,
            &task_executor,
            BLOCKS_TABLE_NAME.to_string(),
            "".to_string(), // No buildername used in blocks table.
            Some(config.disk_database_path),
            Some(config.disk_max_size_mb.unwrap_or(DEFAULT_MAX_DISK_SIZE_MB) * MEGA),
            config
                .memory_max_size_mb
                .unwrap_or(DEFAULT_MAX_MEMORY_SIZE_MB)
                * MEGA,
            BLOCKS_TABLE_NAME,
        );
        // Task to forward the cancellation to the task_manager.
        tokio::spawn(async move {
            cancellation_token.cancelled().await;
            // @Pending: Needed to avoid losing blocks but we should try to avoid this.
            tokio::time::sleep(Duration::from_secs(1)).await;
            task_manager.graceful_shutdown_with_timeout(Duration::from_secs(5));
        });
        Self {
            blocks_tx: block_tx,
        }
    }
}

impl BidObserver for BuiltBlocksWriter {
    fn block_submitted(
        &self,
        slot_data: &MevBoostSlotData,
        submit_block_request: &SubmitBlockRequest,
        built_block_trace: &BuiltBlockTrace,
        builder_name: String,
        best_bid_value: U256,
    ) {
        let submit_trace = submit_block_request.bid_trace();
        let execution_payload_v1 = submit_block_request.execution_payload_v1();
        let block_row = BlockRow {
            block_number: slot_data.block(),
            profit: format_ether(built_block_trace.true_bid_value),
            slot: slot_data.slot(),
            hash: execution_payload_v1.block_hash.to_string(),
            gas_limit: submit_trace.gas_limit,
            gas_used: submit_trace.gas_used,
            base_fee: execution_payload_v1
                .base_fee_per_gas
                .try_into()
                .unwrap_or_default(),
            parent_hash: submit_trace.parent_hash.to_string(),
            proposer_pubkey: "0x123...".to_string(),
            proposer_fee_recipient: "0x456...".to_string(),
            builder_pubkey: "0x789...".to_string(),
            timestamp: 1699999999,
            timestamp_datetime: 1699999999000000,
            orders_closed_at: 1699999998000000,
            sealed_at: 1699999998500000,
            algorithm: "greedy".to_string(),
            true_value: Some(U256::from(123u64)),
            best_relay_value: Some(U256::from(1234u64)),
            block_value: None,
            used_bundle_hashes: vec!["0xbundle1".to_string()],
            used_bundle_uuids: vec!["uuid-1".to_string()],
            used_sbundles_hashes: vec!["0xsbundle1".to_string()],
            delayed_payment_sources: vec!["relay1".to_string()],
            delayed_payment_values: vec![U256::from(123456u64), U256::from(1234567u64)],
            delayed_payment_addresses: vec!["0xaddr1".to_string()],
        };
        let blocks_tx = self.blocks_tx.clone();
        tokio::spawn(async move {
            if let Err(error) = blocks_tx.send(block_row).await {
                error!(?error, "Failed to send block to clickhouse");
            }
        });
    }
}
