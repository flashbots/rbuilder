//! Clickhouse integration to save all the blocks we build and submit to relays,
//! and to record the order journal (add/remove events) to the `available_orders_journal` table.

use alloy_primitives::{utils::format_ether, Address, B256, U256};
use alloy_rpc_types_beacon::relay::SubmitBlockRequest as AlloySubmitBlockRequest;
use clickhouse::{Client, Row};
use dashmap::DashSet;
use rbuilder::{
    building::{
        journal::{
            Error, OrderJournalObserver, OrderJournalObserverFactory, SimulatedOrderJournalCommand,
        },
        BuiltBlockTrace,
    },
    live_builder::{
        block_output::{
            bidding_service_interface::{BidObserver, RelaySet},
            relay_submit::{AlwaysSubmitPolicy, RelaySubmissionPolicy},
        },
        payload_events::MevBoostSlotData,
        simulation::SimulatedOrderCommand,
    },
};
use rbuilder_primitives::{Order, OrderId};
use rbuilder_utils::clickhouse::{
    backup::{
        primitives::{ClickhouseIndexableData, ClickhouseRowExt},
        DiskBackup,
    },
    serde::{option_u256, vec_u256},
    spawn_clickhouse_inserter_and_backup,
};
use rbuilder_utils::tasks::TaskExecutor;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::{collections::HashMap, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::error;

use crate::{
    flashbots_config::BuiltBlocksClickhouseConfig,
    metrics::{
        set_disk_backup_max_size, set_disk_backup_max_size_to_submit_bids_to_relays,
        ClickhouseMetrics, CLICKHOUSE_DISK_BACKUP_SIZE_BYTES,
    },
};

/// BlockRow to insert in clickhouse and also as entry type for the indexer since the BlockRow is made from a few &objects so it makes no sense to have a Block type and copy all the fields.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct BlockRow {
    pub block_number: u64,
    pub block_id: u64,
    /// Number of times this block_id was used by the builder to bid before this.
    /// Starts with 0 on the first bid.
    pub block_uses: u64,
    /// name of the node that submitted the block
    pub builder_name: String,
    /// git commit of the rbuilder running
    pub rbuilder_commit: String,
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
    /// name of the algorithm that created the block
    pub algorithm: String,

    #[serde(with = "option_u256")]
    pub true_value: Option<U256>,
    #[serde(with = "option_u256")]
    pub best_relay_value: Option<U256>,
    #[serde(with = "option_u256")]
    pub block_value: Option<U256>,

    pub used_bundle_hashes: Vec<String>,
    pub used_bundle_uuids: Vec<String>,
    pub delayed_payment_sources: Vec<String>,

    #[serde(with = "vec_u256")]
    pub delayed_payment_values: Vec<U256>,

    pub delayed_payment_addresses: Vec<String>,
    pub sent_to_relay_at: i64,
    pub tx_hashes: Vec<String>,

    /// Info about the bid that triggered the bid to be generated.
    /// Notice this may not be the top bid since a builder lowering it's bid could trigger a new bid
    /// against the new winner.
    /// These 2 should be Option but for clickhouse optimization we use defaults as null values.
    pub triggering_bid_seen_time: i64,
    pub triggering_bid_relay: String,
    /// Info about the top bid among all relays we are trying to beat (best_relay_value)
    /// These 2 should be Option but for clickhouse optimization we use defaults as null values.
    pub bid_to_beat_seen_time: i64,
    pub bid_to_beat_seen_relay: String,
    pub next_journal_sequence_number: u32,
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

/// Enum matching ClickHouse `Enum8('Tx' = 0, 'Bundle' = 1)`.
#[derive(Debug, Clone, Copy, Serialize_repr, Deserialize_repr)]
#[repr(i8)]
pub enum OrderJournalOrderType {
    Tx = 0,
    Bundle = 1,
}

/// Enum matching ClickHouse `Enum8('Add' = 0, 'Remove' = 1)`.
#[derive(Debug, Clone, Copy, Serialize_repr, Deserialize_repr)]
#[repr(i8)]
pub enum OrderJournalOperationType {
    Add = 0,
    Remove = 1,
}

/// Row for the `available_orders_journal` ClickHouse table.
/// Records each Add/Remove order event as it flows through the builder.
///
/// Field order and types must exactly match the ClickHouse schema (validation is off):
///   server_id       LowCardinality(String)
///   slot_number     UInt64
///   parent_block    FixedString(32)
///   order_type      Enum8('Tx' = 0, 'Bundle' = 1)
///   order_hash      FixedString(32)
///   operation_type  Enum8('Add' = 0, 'Remove' = 1)
///   sequence_number UInt32
///   timestamp       DateTime64(6)
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct OrderJournalRow {
    pub server_id: String,
    pub slot_number: u64,
    #[serde(with = "serde_b256_fixed_bytes")]
    pub parent_block: B256,
    pub order_type: OrderJournalOrderType,
    #[serde(with = "serde_b256_fixed_bytes")]
    pub order_hash: B256,
    pub operation_type: OrderJournalOperationType,
    pub sequence_number: u32,
    pub timestamp: i64,
}

/// Serde helper to serialize/deserialize `B256` as `[u8; 32]` for ClickHouse `FixedString(32)`.
///
/// ClickHouse `FixedString(32)` expects exactly 32 raw bytes in RowBinary (no length prefix).
/// Using `serialize_bytes` would emit a varint length prefix + data (matching `String`), which
/// misaligns the binary stream and causes parse errors on subsequent rows.
/// Instead, we delegate to `[u8; 32]`'s serde impl which serializes as a fixed-size tuple.
mod serde_b256_fixed_bytes {
    use alloy_primitives::B256;
    use serde::{self, Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(value: &B256, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes: &[u8; 32] = value.as_ref();
        bytes.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<B256, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes: [u8; 32] = Deserialize::deserialize(deserializer)?;
        Ok(B256::from(bytes))
    }
}

impl OrderJournalRow {
    fn trace_id(&self) -> String {
        format!(
            "{}:{}:{}",
            self.slot_number, self.parent_block, self.sequence_number
        )
    }
}

const ORDER_JOURNAL_TABLE_NAME: &str = "available_orders_journal";

impl ClickhouseRowExt for OrderJournalRow {
    type TraceId = String;
    const TABLE_NAME: &'static str = ORDER_JOURNAL_TABLE_NAME;

    fn trace_id(&self) -> String {
        self.trace_id()
    }

    fn to_row_ref(row: &Self) -> &<Self as Row>::Value<'_> {
        row
    }
}

impl ClickhouseIndexableData for OrderJournalRow {
    type ClickhouseRowType = OrderJournalRow;

    const DATA_NAME: &'static str = <OrderJournalRow as ClickhouseRowExt>::TABLE_NAME;

    fn trace_id(&self) -> String {
        self.trace_id()
    }

    fn to_row(self, _builder_name: String) -> Self::ClickhouseRowType {
        self
    }
}

pub(crate) const KILO: u64 = 1024;
pub(crate) const MEGA: u64 = KILO * KILO;

// Super worst scenario we submit 500 blocks per second so we have 10 seconds of buffer.
// After this having this queued blocks we will start to drop. BlockRow is small enough (in the order of 10K, only hashes/ids, not full orders) so 5K BlockRows is not too much memory.
const BUILT_BLOCKS_CHANNEL_SIZE: usize = 5 * 1024;
const BLOCKS_TABLE_NAME: &str = "blocks";
const DEFAULT_MAX_DISK_SIZE_MB: u64 = 10 * KILO;
pub const DEFAULT_MAX_MEMORY_SIZE_MB: u64 = KILO;
pub const DEFAULT_SEND_TIMEOUT_MS: u64 = 2000;
pub const DEFAULT_END_TIMEOUT_MS: u64 = 3000;
#[derive(Debug)]
pub struct BuiltBlocksWriter {
    blocks_tx: mpsc::Sender<BlockRow>,
    rbuilder_commit: String,
    builder_name: String,
}

/// Policy to stop submitting blocks if the disk backup is too big.
#[derive(Debug)]
struct BackupNotTooBigRelaySubmissionPolicy {
    disk_max_size_to_submit: u64,
}

impl BackupNotTooBigRelaySubmissionPolicy {
    pub fn new(disk_max_size_to_submit: u64) -> Self {
        Self {
            disk_max_size_to_submit,
        }
    }
}

impl RelaySubmissionPolicy for BackupNotTooBigRelaySubmissionPolicy {
    /// We assume should_submit is not called super often so we can look at maps.
    fn should_submit(&self) -> bool {
        (CLICKHOUSE_DISK_BACKUP_SIZE_BYTES
            .with_label_values(&[BLOCKS_TABLE_NAME])
            .get() as u64)
            + (CLICKHOUSE_DISK_BACKUP_SIZE_BYTES
                .with_label_values(&[ORDER_JOURNAL_TABLE_NAME])
                .get() as u64)
            < self.disk_max_size_to_submit
    }
}

/// Creates a [`RelaySubmissionPolicy`] based on the ClickHouse backup config and sets the
/// corresponding disk backup size gauges.
///
/// If `disk_max_size_to_submit_bids_to_relays_mb` is set, returns a policy that stops
/// submitting when the blocks disk backup exceeds that threshold; otherwise returns
/// [`AlwaysSubmitPolicy`].
pub fn create_relay_submission_policy(
    config: &BuiltBlocksClickhouseConfig,
) -> eyre::Result<Box<dyn RelaySubmissionPolicy + Send + Sync>> {
    let backup_max_size_bytes = config.disk_max_size_mb.unwrap_or(DEFAULT_MAX_DISK_SIZE_MB) * MEGA;
    let submission_policy: Box<dyn RelaySubmissionPolicy + Send + Sync> =
        if let Some(disk_max_size_to_submit_bids_to_relays_mb) =
            config.disk_max_size_to_submit_bids_to_relays_mb
        {
            let disk_max_size_to_submit_bids_to_relays =
                disk_max_size_to_submit_bids_to_relays_mb * MEGA;
            if disk_max_size_to_submit_bids_to_relays > backup_max_size_bytes {
                eyre::bail!(
                    "disk_max_size_to_submit_bids_to_relays_mb must be less than disk_max_size_mb"
                );
            }
            set_disk_backup_max_size_to_submit_bids_to_relays(
                disk_max_size_to_submit_bids_to_relays,
            );
            Box::new(BackupNotTooBigRelaySubmissionPolicy::new(
                disk_max_size_to_submit_bids_to_relays,
            ))
        } else {
            Box::new(AlwaysSubmitPolicy {})
        };

    set_disk_backup_max_size(backup_max_size_bytes);

    Ok(submission_policy)
}

impl BuiltBlocksWriter {
    /// Creates the writer and spawns the background ClickHouse inserter + backup tasks.
    /// Returns `(Self, JoinHandle<()>)` where the handle should be awaited during shutdown.
    ///
    /// Accepts pre-created shared ClickHouse infrastructure (`client`, `task_executor`, `disk_backup`)
    /// so the same resources can be shared with other writers (e.g. `OrderJournalWriter`).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: &Client,
        task_executor: &TaskExecutor,
        disk_backup: DiskBackup,
        builder_name: String,
        rbuilder_commit: String,
        memory_max_size_bytes: u64,
        send_timeout: Duration,
        end_timeout: Duration,
    ) -> (Self, JoinHandle<()>) {
        let (block_tx, block_rx) = mpsc::channel::<BlockRow>(BUILT_BLOCKS_CHANNEL_SIZE);

        let clickhouse_shutdown_handle =
            spawn_clickhouse_inserter_and_backup::<BlockRow, BlockRow, ClickhouseMetrics>(
                client,
                block_rx,
                task_executor,
                BLOCKS_TABLE_NAME.to_string(),
                "".to_string(), // No buildername used in blocks table.
                disk_backup,
                memory_max_size_bytes,
                send_timeout,
                end_timeout,
                BLOCKS_TABLE_NAME,
            );

        (
            Self {
                blocks_tx: block_tx,
                rbuilder_commit,
                builder_name,
            },
            clickhouse_shutdown_handle,
        )
    }
}

fn offset_date_to_clickhouse_timestamp(date: OffsetDateTime) -> i64 {
    (date.unix_timestamp_nanos() / 1000) as i64
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
        builder_algorithm_name: String,
        _relays: &RelaySet,
        sent_to_relay_at: OffsetDateTime,
    ) {
        let slot = slot_data.slot();
        let block_number = slot_data.block();
        let blocks_tx = self.blocks_tx.clone();
        let rbuilder_commit = self.rbuilder_commit.clone();
        let builder_name = self.builder_name.clone();
        tokio::spawn(async move {
            // How many times we submitted this block_id to the relay.
            let mut block_uses = HashMap::new();

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
                if let Order::Bundle(bundle) = res.order.as_ref() {
                    used_bundle_hashes
                        .push(bundle.external_hash.unwrap_or(bundle.hash).to_string());
                    used_bundle_uuids.push(bundle.uuid.to_string());
                }
            }
            let (delayed_payment_sources, delayed_payment_values, delayed_payment_addresses) =
                get_delayed_payments(&built_block_trace);
            let delayed_payment_addresses = delayed_payment_addresses
                .iter()
                .map(|address| address.to_string().to_lowercase())
                .collect();
            let tx_hashes = built_block_trace
                .included_orders
                .iter()
                .flat_map(|res| res.tx_infos.iter().map(|info| info.tx.hash().to_string()))
                .collect();
            let block_uses = block_uses
                .entry(built_block_trace.build_block_id)
                .or_insert(0);
            let (triggering_bid_seen_time, triggering_bid_relay) = built_block_trace
                .competition_bid_context
                .triggering_bid_source_info
                .as_ref()
                .map(|info| {
                    (
                        offset_date_to_clickhouse_timestamp(info.seen_time),
                        info.relay.clone(),
                    )
                })
                .unwrap_or_default();
            let (bid_to_beat_seen_time, bid_to_beat_seen_relay, best_relay_value) =
                built_block_trace
                    .competition_bid_context
                    .seen_competition_bid
                    .as_ref()
                    .map(|bid_with_info| {
                        (
                            offset_date_to_clickhouse_timestamp(bid_with_info.info.seen_time),
                            bid_with_info.info.relay.clone(),
                            Some(bid_with_info.value),
                        )
                    })
                    .unwrap_or_default();

            let block_row = BlockRow {
                block_number,
                block_id: built_block_trace.build_block_id.0,
                builder_name,
                rbuilder_commit,
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
                algorithm: builder_algorithm_name,
                true_value: Some(built_block_trace.true_bid_value),
                best_relay_value,
                block_value: Some(submit_trace.value),
                used_bundle_hashes,
                used_bundle_uuids,
                delayed_payment_sources,
                delayed_payment_values,
                delayed_payment_addresses,
                sent_to_relay_at: offset_date_to_clickhouse_timestamp(sent_to_relay_at),
                tx_hashes,
                block_uses: *block_uses,
                triggering_bid_seen_time,
                triggering_bid_relay,
                bid_to_beat_seen_time,
                bid_to_beat_seen_relay,
                next_journal_sequence_number: built_block_trace.next_journal_sequence_number as u32,
            };
            *block_uses += 1;

            if let Err(err) = blocks_tx.try_send(block_row) {
                error!(?err, "Failed to send block to clickhouse");
            }
        });
    }
}

/// Channel size for the order journal. Orders are small and arrive frequently.
const ORDER_JOURNAL_CHANNEL_SIZE: usize = 10 * 1024;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct SlotKey {
    slot_number: u64,
    parent_block: B256,
}

/// Factory that creates OrderJournalWriters for each slot.
#[derive(Debug)]
pub struct OrderJournalWriterFactory {
    journal_tx: mpsc::Sender<OrderJournalRow>,
    builder_name: String,
    /// Alive OrderJournalWriters. Set in OrderJournalWriterFactory::create_observer removed on OrderJournalWriter::drop.
    /// We never allow 2 simultaneous OrderJournalWriters for the same slot.
    active_slots: Arc<DashSet<SlotKey>>,
}

impl OrderJournalWriterFactory {
    /// Creates the writer and spawns the background ClickHouse inserter + backup tasks.
    /// Returns `(Self, JoinHandle<()>)` where the handle should be awaited during shutdown.
    pub fn new(
        client: &Client,
        task_executor: &TaskExecutor,
        disk_backup: DiskBackup,
        builder_name: String,
        memory_max_size_bytes: u64,
        send_timeout: Duration,
        end_timeout: Duration,
    ) -> (Self, JoinHandle<()>) {
        let (journal_tx, journal_rx) = mpsc::channel::<OrderJournalRow>(ORDER_JOURNAL_CHANNEL_SIZE);

        let shutdown_handle = spawn_clickhouse_inserter_and_backup::<
            OrderJournalRow,
            OrderJournalRow,
            ClickhouseMetrics,
        >(
            client,
            journal_rx,
            task_executor,
            ORDER_JOURNAL_TABLE_NAME.to_string(),
            "".to_string(),
            disk_backup,
            memory_max_size_bytes,
            send_timeout,
            end_timeout,
            ORDER_JOURNAL_TABLE_NAME,
        );

        (
            Self {
                journal_tx,
                builder_name,
                active_slots: Arc::new(DashSet::new()),
            },
            shutdown_handle,
        )
    }
}

impl OrderJournalObserverFactory for OrderJournalWriterFactory {
    fn create_observer(
        &self,
        slot_data: &MevBoostSlotData,
    ) -> Result<Box<dyn OrderJournalObserver + Send + Sync>, Error> {
        let slot_key = SlotKey {
            slot_number: slot_data.slot(),
            parent_block: slot_data.parent_block_hash(),
        };
        if !self.active_slots.insert(slot_key.clone()) {
            return Err(Error::SlotAlreadyInProgress);
        }
        let writer = OrderJournalWriter {
            journal_tx: self.journal_tx.clone(),
            builder_name: self.builder_name.clone(),
            slot_key,
            active_slots: self.active_slots.clone(),
        };
        Ok(Box::new(writer))
    }
}

/// Writes order journal events (Add/Remove) to the `available_orders_journal` ClickHouse table.
#[derive(Debug)]
struct OrderJournalWriter {
    journal_tx: mpsc::Sender<OrderJournalRow>,
    builder_name: String,
    slot_key: SlotKey,
    active_slots: Arc<DashSet<SlotKey>>,
}

impl OrderJournalObserver for OrderJournalWriter {
    fn order_delivered(&self, command: &SimulatedOrderJournalCommand) {
        let (order_id, operation_type) = match command.command() {
            SimulatedOrderCommand::Simulation(sim_order) => {
                (sim_order.id(), OrderJournalOperationType::Add)
            }
            SimulatedOrderCommand::Cancellation(id) => (*id, OrderJournalOperationType::Remove),
        };

        let order_type = match order_id {
            OrderId::Tx(_) => OrderJournalOrderType::Tx,
            OrderId::Bundle(_) => OrderJournalOrderType::Bundle,
        };

        let row = OrderJournalRow {
            server_id: self.builder_name.clone(),
            slot_number: self.slot_key.slot_number,
            parent_block: self.slot_key.parent_block,
            order_type,
            order_hash: order_id.fixed_bytes(),
            operation_type,
            sequence_number: command.sequence_number() as u32,
            timestamp: (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1000) as i64,
        };

        if let Err(err) = self.journal_tx.try_send(row) {
            error!(?err, "Failed to send order journal row to clickhouse");
        }
    }
}

impl Drop for OrderJournalWriter {
    fn drop(&mut self) {
        self.active_slots.remove(&self.slot_key);
    }
}
