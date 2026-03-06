pub mod backup;
pub mod indexer;
pub mod serde;
use std::time::Duration;

use ::serde::{Deserialize, Serialize};
use clickhouse::Client;
use reth_tasks::TaskExecutor;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::clickhouse::{
    backup::{
        metrics::Metrics,
        primitives::{ClickhouseIndexableData, ClickhouseRowExt},
        Backup, DiskBackup, MemoryBackupConfig,
    },
    indexer::{default_inserter, ClickhouseInserter, InserterRunner},
};

/// Equilalent of `clickhouse::inserter::Quantities` with more traits derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Quantities {
    pub bytes: u64,
    pub rows: u64,
    pub transactions: u64,
}

impl Quantities {
    /// Just zero quantities, nothing special.
    pub const ZERO: Quantities = Quantities {
        bytes: 0,
        rows: 0,
        transactions: 0,
    };
}

impl From<clickhouse::inserter::Quantities> for Quantities {
    fn from(value: clickhouse::inserter::Quantities) -> Self {
        Self {
            bytes: value.bytes,
            rows: value.rows,
            transactions: value.transactions,
        }
    }
}

impl From<Quantities> for clickhouse::inserter::Quantities {
    fn from(value: Quantities) -> Self {
        Self {
            bytes: value.bytes,
            rows: value.rows,
            transactions: value.transactions,
        }
    }
}

/// Size of the channel buffer for the backup input channel.
/// If we get more than this number of failed commits queued the inserter thread will block.
const BACKUP_INPUT_CHANNEL_BUFFER_SIZE: usize = 128;

/// Main func to spawn the clickhouse inserter and backup tasks.
/// Returns a JoinHandle that resolves when both tasks have completed.
#[allow(clippy::too_many_arguments)]
pub fn spawn_clickhouse_inserter_and_backup<
    DataType: ClickhouseIndexableData + Send + Sync + 'static,
    RowType: ClickhouseRowExt,
    MetricsType: Metrics + Send + Sync + 'static,
>(
    client: &Client,
    data_rx: mpsc::Receiver<DataType>,
    task_executor: &TaskExecutor,
    clickhouse_table_name: String,
    builder_name: String,
    disk_backup_db: DiskBackup,
    memory_max_size_bytes: u64,
    send_timeout: Duration,
    end_timeout: Duration,
    tracing_target: &'static str,
) -> JoinHandle<()>
where
    for<'a> <DataType::ClickhouseRowType as clickhouse::Row>::Value<'a>: Sync,
{
    let backup_table_name = RowType::TABLE_NAME.to_string();
    let (failed_commit_tx, failed_commit_rx) = mpsc::channel(BACKUP_INPUT_CHANNEL_BUFFER_SIZE);
    let inserter = default_inserter(client, &clickhouse_table_name, send_timeout, end_timeout);
    let inserter = ClickhouseInserter::<_, MetricsType>::new(inserter, failed_commit_tx);
    // Node name is not used for Blocks.
    let inserter_runner = InserterRunner::new(data_rx, inserter, builder_name);

    let backup = Backup::<_, MetricsType>::new(
        failed_commit_rx,
        client
            .inserter(&clickhouse_table_name)
            .with_timeouts(Some(send_timeout), Some(end_timeout)),
        disk_backup_db,
    )
    .with_memory_backup_config(MemoryBackupConfig::new(memory_max_size_bytes));

    let inserter_handle =
        inserter_runner.spawn(task_executor, backup_table_name.clone(), tracing_target);
    let backup_handle = backup.spawn(task_executor, backup_table_name, tracing_target);

    // Spawn a wrapper task that waits for both to complete
    tokio::spawn(async move {
        let _ = tokio::join!(inserter_handle, backup_handle);
    })
}
