//! Indexing functionality powered by Clickhouse.

use std::{
    fmt::Debug,
    time::{Duration, Instant},
};

use clickhouse::{
    error::Result as ClickhouseResult,
    inserter::{Inserter, Quantities},
    Client as ClickhouseClient, Row,
};
use tokio::sync::mpsc;

use crate::clickhouse_with_backup::primitives::{ClickhouseIndexableOrder, ClickhouseRowExt};

mod backup;
pub(crate) mod primitives;

/// An clickhouse inserter with some sane defaults.
fn default_inserter<T: Row>(client: &ClickhouseClient, table_name: &str) -> Inserter<T> {
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
struct ClickhouseInserter<T: ClickhouseRowExt> {
    /// The inner Clickhouse inserter client.
    inner: Inserter<T>,
    /// A small in-memory backup of the current data we're trying to commit. In case this fails to
    /// be inserted into Clickhouse, it is sent to the backup actor.
    rows_backup: Vec<T>,
    /// The channel where to send data to be backed up.
    backup_tx: mpsc::Sender<FailedCommit<T>>,
}

impl<T: ClickhouseRowExt> ClickhouseInserter<T> {
    fn new(inner: Inserter<T>, backup_tx: mpsc::Sender<FailedCommit<T>>) -> Self {
        let rows_backup = Vec::new();
        Self {
            inner,
            rows_backup,
            backup_tx,
        }
    }

    /// Writes the provided order into the inner Clickhouse writer buffer.
    async fn write(&mut self, row: T) {
        let hash = row.hash();
        let value_ref = ClickhouseRowExt::to_row_ref(&row);

        if let Err(e) = self.inner.write(value_ref).await {
            IndexerMetrics::increment_clickhouse_write_failures(e.to_string());
            tracing::error!(target: TARGET, order = T::ORDER, ?e, %hash, "failed to write to clickhouse inserter");
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
                    tracing::trace!(target: TARGET, order = T::ORDER, "committed to inserter");
                } else {
                    tracing::debug!(target: TARGET, order = T::ORDER, ?quantities, "inserted batch to clickhouse");
                    IndexerMetrics::process_clickhouse_quantities(&quantities.into());
                    IndexerMetrics::record_clickhouse_batch_commit_time(start.elapsed());
                    // Clear the backup rows.
                    self.rows_backup.clear();
                }
            }
            Err(e) => {
                IndexerMetrics::increment_clickhouse_commit_failures(e.to_string());
                tracing::error!(target: TARGET, order = T::ORDER, ?e, "failed to commit bundle to clickhouse");

                let rows = std::mem::take(&mut self.rows_backup);
                let failed_commit = FailedCommit::new(rows, pending);

                if let Err(e) = self.backup_tx.try_send(failed_commit) {
                    tracing::error!(target: TARGET, order = T::ORDER, ?e, "failed to send rows backup");
                }
            }
        }
    }

    /// Ends the current `INSERT` and whole `Inserter` unconditionally.
    async fn end(self) -> ClickhouseResult<Quantities> {
        self.inner.end().await.map(Into::into)
    }
}

impl<T: ClickhouseRowExt> std::fmt::Debug for ClickhouseInserter<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickhouseInserter")
            .field("inserter", &T::ORDER.to_string())
            .field("rows_backup_len", &self.rows_backup.len())
            .finish()
    }
}

/// A long-lived actor to run a [`ClickhouseIndexer`] until it possible to receive new order to
/// index.
struct InserterRunner<T: ClickhouseIndexableOrder> {
    /// The channel from which we can receive new orders to index.
    rx: mpsc::Receiver<T>,
    /// The underlying Clickhouse inserter.
    inserter: ClickhouseInserter<T::ClickhouseRowType>,
    /// The name of the local operator to use when adding data to clickhouse.
    builder_name: String,
}

impl<T: ClickhouseIndexableOrder> InserterRunner<T> {
    fn new(
        rx: mpsc::Receiver<T>,
        inserter: ClickhouseInserter<T::ClickhouseRowType>,
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
            tracing::trace!(target: TARGET, order = T::ORDER, hash = %order.hash(), "received data to index");
            sampler.sample(|| {
                IndexerMetrics::set_clickhouse_queue_size(self.rx.len(), T::ORDER);
            });

            let row = order.to_row(self.builder_name.clone());
            self.inserter.write(row).await;
            self.inserter.commit().await;
        }
        tracing::error!(target: TARGET, order = T::ORDER, "tx channel closed, indexer will stop running");
    }
}

/// The configuration used in a [`ClickhouseClient`].
#[derive(Debug, Clone)]
pub(crate) struct ClickhouseClientConfig {
    host: String,
    database: String,
    username: String,
    password: String,
    validation: bool,
}

impl ClickhouseClientConfig {
    fn new(args: &ClickhouseArgs, validation: bool) -> Self {
        Self {
            host: args.host.clone().expect("host is set"),
            database: args.database.clone().expect("database is set"),
            username: args.username.clone().expect("username is set"),
            password: args.password.clone().expect("password is set"),
            validation,
        }
    }
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
