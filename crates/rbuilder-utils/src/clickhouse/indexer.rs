//! Indexing functionality powered by Clickhouse.

use std::{
    fmt::Debug,
    time::{Duration, Instant},
};

/// The tracing target for this indexer crate. @PendingDX REMOVE
const TARGET: &str = "indexer";

use clickhouse::{
    error::Result as ClickhouseResult, inserter::Inserter, Client as ClickhouseClient, Row,
};
use reth_tasks::TaskExecutor;
use tokio::sync::mpsc;

use crate::{
    clickhouse::{
        backup::{
            metrics::Metrics,
            primitives::{ClickhouseIndexableData, ClickhouseRowExt},
            FailedCommit,
        },
        Quantities,
    },
    metrics::Sampler,
};

/// A default maximum size in bytes for the in-memory backup of failed commits.
pub const MAX_MEMORY_BACKUP_SIZE_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB
/// A default maximum size in bytes for the disk backup of failed commits.
pub const MAX_DISK_BACKUP_SIZE_BYTES: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB

/// The default path where the backup database is stored. For tests, a temporary file is used.
pub fn default_disk_backup_database_path() -> String {
    #[cfg(test)]
    return tempfile::NamedTempFile::new()
        .unwrap()
        .path()
        .to_string_lossy()
        .to_string();
    #[cfg(not(test))]
    {
        use std::path::PathBuf;

        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".buildernet-orderflow-proxy")
            .join("clickhouse_backup.db")
            .to_string_lossy()
            .to_string()
    }
}

/// An clickhouse inserter with some sane defaults.
pub fn default_inserter<T: Row>(client: &ClickhouseClient, table_name: &str) -> Inserter<T> {
    // TODO: make this configurable.
    let send_timeout = Duration::from_secs(2);
    let end_timeout = Duration::from_secs(3);

    client
        .inserter::<T>(table_name)
        .with_period(Some(Duration::from_secs(4))) // Dump every 4s
        .with_period_bias(0.1) // 4±(0.1*4)
        .with_max_bytes(128 * 1024 * 1024) // 128MiB
        .with_max_rows(65_536)
        .with_timeouts(Some(send_timeout), Some(end_timeout))
}

/// A wrapper over a Clickhouse [`Inserter`] that supports a backup mechanism.
pub struct ClickhouseInserter<T: ClickhouseRowExt, MetricsType> {
    /// The inner Clickhouse inserter client.
    inner: Inserter<T>,
    /// A small in-memory backup of the current data we're trying to commit. In case this fails to
    /// be inserted into Clickhouse, it is sent to the backup actor.
    rows_backup: Vec<T>,
    /// The channel where to send data to be backed up.
    backup_tx: mpsc::Sender<FailedCommit<T>>,
    _metrics_phantom: std::marker::PhantomData<MetricsType>,
}

impl<T: ClickhouseRowExt + Send + Sync + 'static, MetricsType: Metrics>
    ClickhouseInserter<T, MetricsType>
{
    pub fn new(inner: Inserter<T>, backup_tx: mpsc::Sender<FailedCommit<T>>) -> Self {
        let rows_backup = Vec::new();
        Self {
            inner,
            rows_backup,
            backup_tx,
            _metrics_phantom: std::marker::PhantomData,
        }
    }

    /// Writes the provided order into the inner Clickhouse writer buffer.
    async fn write(&mut self, row: T) {
        let trace_id = row.trace_id();
        let value_ref = ClickhouseRowExt::to_row_ref(&row);

        if let Err(e) = self.inner.write(value_ref).await {
            MetricsType::increment_write_failures(e.to_string());
            tracing::error!(target: TARGET, table = T::TABLE_NAME, ?e, %trace_id, "failed to write to clickhouse inserter");
            return;
        }

        // NOTE: we don't backup if writing failes. The reason is that if this fails, then the same
        // writing to the backup inserter should fail.
        self.rows_backup.push(row);
    }

    /// Tries to commit to Clickhouse if the conditions are met. In case of failures, data is sent
    /// to the backup actor for retries.
    async fn commit(&mut self) {
        let pending = self.inner.pending().clone().into(); // This is cheap to clone.

        let start = Instant::now();
        match self.inner.commit().await {
            Ok(quantities) => {
                if quantities == Quantities::ZERO.into() {
                    tracing::trace!(target: TARGET, table = T::TABLE_NAME, "committed to inserter");
                } else {
                    tracing::debug!(target: TARGET, table = T::TABLE_NAME, ?quantities, "inserted batch to clickhouse");
                    MetricsType::process_quantities(&quantities.into());
                    MetricsType::record_batch_commit_time(start.elapsed());
                    // Clear the backup rows.
                    self.rows_backup.clear();
                }
            }
            Err(e) => {
                MetricsType::increment_commit_failures(e.to_string());
                tracing::error!(target: TARGET, table = T::TABLE_NAME, ?e, "failed to commit bundle to clickhouse");

                let rows = std::mem::take(&mut self.rows_backup);
                let failed_commit = FailedCommit::new(rows, pending);

                if let Err(e) = self.backup_tx.try_send(failed_commit) {
                    tracing::error!(target: TARGET, table = T::TABLE_NAME, ?e, "failed to send rows backup");
                }
            }
        }
    }

    /// Ends the current `INSERT` and whole `Inserter` unconditionally.
    pub async fn end(self) -> ClickhouseResult<Quantities> {
        self.inner.end().await.map(Into::into)
    }
}

impl<T: ClickhouseRowExt, MetricsType> std::fmt::Debug for ClickhouseInserter<T, MetricsType> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickhouseInserter")
            .field("inserter", &T::TABLE_NAME.to_string())
            .field("rows_backup_len", &self.rows_backup.len())
            .finish()
    }
}

/// A long-lived actor to run a [`ClickhouseIndexer`] until it possible to receive new order to
/// index.
pub struct InserterRunner<T: ClickhouseIndexableData, MetricsType: Metrics> {
    /// The channel from which we can receive new orders to index.
    rx: mpsc::Receiver<T>,
    /// The underlying Clickhouse inserter.
    inserter: ClickhouseInserter<T::ClickhouseRowType, MetricsType>,
    /// The name of the local operator to use when adding data to clickhouse.
    builder_name: String,
}

impl<T: ClickhouseIndexableData, MetricsType: Metrics> std::fmt::Debug
    for InserterRunner<T, MetricsType>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InserterRunner")
            .field("inserter", &T::DATA_NAME.to_string())
            .field("rx", &self.rx)
            .finish()
    }
}

impl<T: ClickhouseIndexableData, MetricsType: Metrics> InserterRunner<T, MetricsType> {
    pub fn new(
        rx: mpsc::Receiver<T>,
        inserter: ClickhouseInserter<T::ClickhouseRowType, MetricsType>,
        builder_name: String,
    ) -> Self {
        Self {
            rx,
            inserter,
            builder_name,
        }
    }

    /// Run the inserter until it is possible to receive new orders.
    async fn run_loop(&mut self) {
        let mut sampler = Sampler::default()
            .with_sample_size(self.rx.capacity() / 2)
            .with_interval(Duration::from_secs(4));

        while let Some(order) = self.rx.recv().await {
            tracing::trace!(target: TARGET, table = T::DATA_NAME, hash = %order.trace_id(), "received data to index");
            sampler.sample(|| {
                MetricsType::set_queue_size(self.rx.len(), T::DATA_NAME);
            });

            let row = order.to_row(self.builder_name.clone());
            self.inserter.write(row).await;
            self.inserter.commit().await;
        }
        tracing::error!(target: TARGET, table = T::DATA_NAME, "tx channel closed, indexer will stop running");
    }

    async fn end(self) -> ClickhouseResult<Quantities> {
        self.inserter.end().await
    }

    /// Spawns the inserter runner on the given task executor.
    pub fn spawn(mut self, task_executor: &TaskExecutor, name: String, target: &'static str)
    where
        T: Send + Sync + 'static,
        MetricsType: Send + Sync + 'static,
        for<'a> <T::ClickhouseRowType as Row>::Value<'a>: Sync,
    {
        task_executor.spawn_with_graceful_shutdown_signal(|shutdown| async move {
            let mut shutdown_guard = None;
            tokio::select! {
                _ = self.run_loop() => {
                    tracing::info!(target,table_name = name, "Clickhouse indexer channel closed");
                }
                guard = shutdown => {
                    tracing::info!(target,table_name = name, "Received shutdown for indexer, performing cleanup");
                    shutdown_guard = Some(guard);
                },
            }

            match self.end().await {
                Ok(quantities) => {
                    tracing::info!(target, ?quantities, table_name = name, "Finalized clickhouse inserter");
                }
                Err(e) => {
                    tracing::error!(target,error = ?e, table_name = name, "Failed to write end insertion of indexer");
                }
            }
            drop(shutdown_guard);

        });
    }
}

/// The configuration used in a [`ClickhouseClient`].
#[derive(Debug, Clone)]
pub struct ClickhouseClientConfig {
    pub host: String,
    pub database: String,
    pub username: String,
    pub password: String,
    pub validation: bool,
}

impl From<ClickhouseClientConfig> for ClickhouseClient {
    fn from(config: ClickhouseClientConfig) -> Self {
        ClickhouseClient::default()
            .with_url(config.host)
            .with_database(config.database)
            .with_user(config.username)
            .with_password(config.password)
            .with_validation(config.validation)
    }
}
