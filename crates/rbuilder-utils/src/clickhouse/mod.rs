pub mod backup;
pub mod indexer;
pub mod serde;
use std::{path::PathBuf, time::Duration};

use ::serde::{Deserialize, Serialize};
use clickhouse::Client;
use reth_tasks::TaskExecutor;
use tokio::sync::mpsc;

use crate::clickhouse::{
    backup::{
        metrics::Metrics,
        primitives::{ClickhouseIndexableData, ClickhouseRowExt},
        Backup, DiskBackup, DiskBackupConfig, MemoryBackupConfig,
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
const CLICKHOUSE_INSERT_TIMEOUT: Duration = Duration::from_secs(2);
const CLICKHOUSE_END_TIMEOUT: Duration = Duration::from_secs(4);

/// Main func to spawn the clickhouse inserter and backup tasks.
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
    disk_database_path: Option<impl Into<PathBuf>>,
    disk_max_size_bytes: Option<u64>,
    memory_max_size_bytes: u64,
    tracing_target: &'static str,
) where
    for<'a> <DataType::ClickhouseRowType as clickhouse::Row>::Value<'a>: Sync,
{
    let backup_table_name = RowType::TABLE_NAME.to_string();
    let disk_backup = DiskBackup::new(
        DiskBackupConfig::new()
            .with_path(disk_database_path)
            .with_max_size_bytes(disk_max_size_bytes), // 1 GiB
        task_executor,
    )
    .expect("could not create disk backup");
    let (failed_commit_tx, failed_commit_rx) = mpsc::channel(BACKUP_INPUT_CHANNEL_BUFFER_SIZE);
    let inserter = default_inserter(client, &clickhouse_table_name);
    let inserter = ClickhouseInserter::<_, MetricsType>::new(inserter, failed_commit_tx);
    // Node name is not used for Blocks.
    let inserter_runner = InserterRunner::new(data_rx, inserter, builder_name);

    let backup = Backup::<_, MetricsType>::new(
        failed_commit_rx,
        client.inserter(&clickhouse_table_name).with_timeouts(
            Some(CLICKHOUSE_INSERT_TIMEOUT),
            Some(CLICKHOUSE_END_TIMEOUT),
        ),
        disk_backup.clone(),
    )
    .with_memory_backup_config(MemoryBackupConfig::new(memory_max_size_bytes));
    inserter_runner.spawn(task_executor, backup_table_name.clone(), tracing_target);
    backup.spawn(task_executor, backup_table_name, tracing_target);
}
