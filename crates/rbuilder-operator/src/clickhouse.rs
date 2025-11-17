//! Clickhouse integration to save all the blocks we build and submit to relays.

use std::{sync::Arc, time::Duration};

use alloy_primitives::{utils::format_ether, Address, U256};
use alloy_rpc_types_beacon::relay::SubmitBlockRequest as AlloySubmitBlockRequest;
use clickhouse::{Client, Row};
use rbuilder::{
    building::BuiltBlockTrace,
    live_builder::{
        block_output::bidding_service_interface::{BidObserver, RelaySet},
        payload_events::MevBoostSlotData,
        process_killer::RUN_SUBMIT_TO_RELAYS_JOB_CANCEL_TIME,
    },
};
use rbuilder_primitives::{Order, OrderId};
use rbuilder_utils::clickhouse::{
    backup::primitives::{ClickhouseIndexableData, ClickhouseRowExt},
    serde::{option_u256, vec_u256},
    spawn_clickhouse_inserter_and_backup,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::error;

use crate::{flashbots_config::BuiltBlocksClickhouseConfig, metrics::ClickhouseMetrics};

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

// Super worst scenario we submit 500 blocks per second so we have 10 seconds of buffer.
// After this having this queued blocks we will start to drop. BlockRow is small enough (in the order of 10K, only hashes/ids, not full orders) so 5K BlockRows is not too much memory.
const BUILT_BLOCKS_CHANNEL_SIZE: usize = 5 * 1024;
const BLOCKS_TABLE_NAME: &str = "blocks";
const DEFAULT_MAX_DISK_SIZE_MB: u64 = 10 * KILO;
const DEFAULT_MAX_MEMORY_SIZE_MB: u64 = KILO;
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
        spawn_clickhouse_inserter_and_backup::<BlockRow, BlockRow, ClickhouseMetrics>(
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
            tokio::time::sleep(RUN_SUBMIT_TO_RELAYS_JOB_CANCEL_TIME).await;
            task_manager.graceful_shutdown_with_timeout(Duration::from_secs(5));
        });
        Self {
            blocks_tx: block_tx,
        }
    }
}

fn offset_date_to_clickhouse_timestamp(date: OffsetDateTime) -> i64 {
    (date.unix_timestamp_nanos() / 1000) as i64
}

fn get_used_sbundles_hashes(built_block_trace: &BuiltBlockTrace) -> Vec<String> {
    built_block_trace
        .included_orders
        .iter()
        .flat_map(|exec_result| {
            if let Order::ShareBundle(sbundle) = &exec_result.order {
                // don't like having special cases (merged vs not merged), can we improve this?
                if sbundle.is_merged_order() {
                    exec_result
                        .original_order_ids
                        .iter()
                        .map(|id| id.to_string())
                        .collect()
                } else if exec_result.tx_infos.is_empty() {
                    // non merged empty execution sbundle
                    vec![]
                } else {
                    // non merged non empty execution sbundle
                    vec![exec_result.order.id().to_string()]
                }
            } else {
                Vec::new()
            }
        })
        .collect()
}

const MEV_VIRTUAL_BLOCKER_SOURCE: &str = "mev_blocker";
const MEV_VIRTUAL_ADDRESS: Address = Address::ZERO;

/// (sources, values, addresses)
fn get_delayed_payments(
    built_block_trace: &BuiltBlockTrace,
) -> (Vec<String>, Vec<U256>, Vec<Address>) {
    let mut sources = Vec::new();
    let mut values = Vec::new();
    let mut addresses = Vec::new();
    for res in &built_block_trace.included_orders {
        if let Some(delayed_kickback) = &res.delayed_kickback {
            if !delayed_kickback.should_pay_in_block {
                match res.order.id() {
                    OrderId::Bundle(uuid) => {
                        sources.push(uuid.to_string());
                        values.push(delayed_kickback.payout_value);
                        addresses.push(delayed_kickback.recipient);
                    }
                    _ => {
                        error!(order = ?res.order.id(), "Delayed kickback is found for non-bundle");
                    }
                }
            }
        }
    }
    sources.push(MEV_VIRTUAL_BLOCKER_SOURCE.into());
    values.push(built_block_trace.mev_blocker_price);
    addresses.push(MEV_VIRTUAL_ADDRESS);
    (sources, values, addresses)
}

impl BidObserver for BuiltBlocksWriter {
    fn block_submitted(
        &self,
        slot_data: &MevBoostSlotData,
        submit_block_request: Arc<AlloySubmitBlockRequest>,
        built_block_trace: Arc<BuiltBlockTrace>,
        builder_name: String,
        best_bid_value: U256,
        _relays: &RelaySet,
    ) {
        let slot = slot_data.slot();
        let block_number = slot_data.block();
        let blocks_tx = self.blocks_tx.clone();
        tokio::spawn(async move {
            let submit_trace = submit_block_request.bid_trace();
            let execution_payload_v1 = match submit_block_request.as_ref() {
                AlloySubmitBlockRequest::Capella(request) => {
                    &request.execution_payload.payload_inner
                }
                AlloySubmitBlockRequest::Deneb(request) => {
                    &request.execution_payload.payload_inner.payload_inner
                }
                AlloySubmitBlockRequest::Electra(request) => {
                    &request.execution_payload.payload_inner.payload_inner
                }
                AlloySubmitBlockRequest::Fulu(request) => {
                    &request.execution_payload.payload_inner.payload_inner
                }
            };
            let mut used_bundle_hashes = Vec::new();
            let mut used_bundle_uuids = Vec::new();
            for res in &built_block_trace.included_orders {
                if let Order::Bundle(bundle) = &res.order {
                    used_bundle_hashes
                        .push(bundle.external_hash.unwrap_or(bundle.hash).to_string());
                    used_bundle_uuids.push(bundle.uuid.to_string());
                }
            }
            let used_sbundles_hashes = get_used_sbundles_hashes(&built_block_trace);
            let (delayed_payment_sources, delayed_payment_values, delayed_payment_addresses) =
                get_delayed_payments(&built_block_trace);
            let delayed_payment_addresses = delayed_payment_addresses
                .iter()
                .map(|address| address.to_string().to_lowercase())
                .collect();
            let block_row = BlockRow {
                block_number,
                profit: format_ether(submit_trace.value),
                slot,
                hash: execution_payload_v1.block_hash.to_string(),
                gas_limit: submit_trace.gas_limit,
                gas_used: submit_trace.gas_used,
                base_fee: execution_payload_v1
                    .base_fee_per_gas
                    .try_into()
                    .unwrap_or_default(),
                parent_hash: submit_trace.parent_hash.to_string(),
                proposer_pubkey: submit_trace.proposer_pubkey.to_string(),
                proposer_fee_recipient: submit_trace.proposer_fee_recipient.to_string(),
                builder_pubkey: submit_trace.builder_pubkey.to_string(),
                timestamp: execution_payload_v1.timestamp,
                timestamp_datetime: execution_payload_v1.timestamp as i64 * 1_000_000,
                orders_closed_at: offset_date_to_clickhouse_timestamp(
                    built_block_trace.orders_closed_at,
                ),
                sealed_at: offset_date_to_clickhouse_timestamp(built_block_trace.orders_sealed_at),
                algorithm: builder_name,
                true_value: Some(built_block_trace.true_bid_value),
                best_relay_value: Some(best_bid_value),
                block_value: Some(submit_trace.value),
                used_bundle_hashes,
                used_bundle_uuids,
                used_sbundles_hashes,
                delayed_payment_sources,
                delayed_payment_values,
                delayed_payment_addresses,
            };
            if let Err(err) = blocks_tx.try_send(block_row) {
                error!(?err, "Failed to send block to clickhouse");
            }
        });
    }
}
