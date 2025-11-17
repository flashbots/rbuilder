pub mod metrics;
pub mod primitives;

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clickhouse::inserter::Inserter;
use derive_more::{Deref, DerefMut};
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata};
use strum::AsRefStr;
use tokio::sync::mpsc;

use crate::{
    backoff::BackoffInterval,
    clickhouse::{
        backup::{
            metrics::Metrics,
            primitives::{ClickhouseIndexableData, ClickhouseRowExt},
        },
        indexer::{
            default_disk_backup_database_path, MAX_DISK_BACKUP_SIZE_BYTES,
            MAX_MEMORY_BACKUP_SIZE_BYTES,
        },
        Quantities,
    },
    format::FormatBytes,
    tasks::TaskExecutor,
};

const TARGET: &str = "clickhouse_with_backup::backup";

/// A type alias for disk backup keys.
type DiskBackupKey = u128;
/// A type alias for disk backup tables.
type Table<'a> = redb::TableDefinition<'a, DiskBackupKey, Vec<u8>>;

/// The source of a backed-up failed commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackupSource {
    Disk(DiskBackupKey),
    Memory,
}

/// Generates a new unique key for disk backup entries, based on current system time in
/// milliseconds.
fn new_disk_backup_key() -> DiskBackupKey {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_micros()
}

/// Represents data we failed to commit to clickhouse, including the rows and some information
/// about the size of such data.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FailedCommit<T> {
    /// The actual rows we were trying to commit.
    rows: Vec<T>,
    /// The quantities related to such commit, like the total size in bytes.
    quantities: Quantities,
}

impl<T> FailedCommit<T> {
    pub fn new(rows: Vec<T>, quantities: Quantities) -> Self {
        Self { rows, quantities }
    }
}

impl<T: ClickhouseIndexableData> Default for FailedCommit<T> {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            quantities: Quantities::ZERO,
        }
    }
}

/// A [`FailedCommit`] along with its source (disk or memory).
struct RetrievedFailedCommit<T> {
    source: BackupSource,
    commit: FailedCommit<T>,
}

/// A wrapper over a [`VecDeque`] of [`FailedCommit`] with added functionality.
///
/// Newly failed commits are pushed to the front of the queue, so the oldest are at the back.
#[derive(Deref, DerefMut)]
struct FailedCommits<T>(VecDeque<FailedCommit<T>>);

impl<T> FailedCommits<T> {
    /// Get the aggregated quantities of the failed commits;
    #[inline]
    fn quantities(&self) -> Quantities {
        let total_size_bytes = self.iter().map(|c| c.quantities.bytes).sum::<u64>();
        let total_rows = self.iter().map(|c| c.quantities.rows).sum::<u64>();
        let total_transactions = self.iter().map(|c| c.quantities.transactions).sum::<u64>();

        Quantities {
            bytes: total_size_bytes,
            rows: total_rows,
            transactions: total_transactions,
        }
    }
}

impl<T> Default for FailedCommits<T> {
    fn default() -> Self {
        Self(VecDeque::default())
    }
}

/// Configuration for the [`DiskBackup`] of failed commits.
#[derive(Debug)]
pub struct DiskBackupConfig {
    /// The path where the backup database is stored.
    path: PathBuf,
    /// The maximum size in bytes for holding past failed commits on disk.
    max_size_bytes: u64,
    /// The interval at which buffered writes are flushed to disk.
    flush_interval: tokio::time::Interval,
}

impl DiskBackupConfig {
    pub fn new() -> Self {
        Self {
            path: default_disk_backup_database_path().into(),
            max_size_bytes: MAX_DISK_BACKUP_SIZE_BYTES,
            flush_interval: tokio::time::interval(Duration::from_secs(30)),
        }
    }

    pub fn with_path<P: Into<PathBuf>>(mut self, path: Option<P>) -> Self {
        if let Some(p) = path {
            self.path = p.into();
        }
        self
    }

    pub fn with_max_size_bytes(mut self, max_size_bytes: Option<u64>) -> Self {
        if let Some(max_size_bytes) = max_size_bytes {
            self.max_size_bytes = max_size_bytes;
        }
        self
    }

    #[allow(dead_code)]
    pub fn with_immediate_commit_interval(mut self, interval: Option<Duration>) -> Self {
        if let Some(interval) = interval {
            self.flush_interval = tokio::time::interval(interval);
        }
        self
    }
}

impl Default for DiskBackupConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for DiskBackupConfig {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            max_size_bytes: self.max_size_bytes,
            flush_interval: tokio::time::interval(self.flush_interval.period()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryBackupConfig {
    /// The maximum size in bytes for holding past failed commits in-memory. Once we go over this
    /// threshold, pressure is applied and old commits are dropped.
    pub max_size_bytes: u64,
}

impl MemoryBackupConfig {
    pub fn new(max_size_bytes: u64) -> Self {
        Self { max_size_bytes }
    }
}

impl Default for MemoryBackupConfig {
    fn default() -> Self {
        Self {
            max_size_bytes: MAX_MEMORY_BACKUP_SIZE_BYTES,
        }
    }
}

/// Data retrieved from disk, along with its key and some stats.
pub(crate) struct DiskRetrieval<K, V> {
    pub(crate) key: K,
    pub(crate) value: V,
    pub(crate) stats: BackupSourceStats,
}

/// Errors that can occur during disk backup operations. Mostly wrapping redb and serde errors.
#[derive(Debug, thiserror::Error, AsRefStr)]
pub(crate) enum DiskBackupError {
    #[error(transparent)]
    Database(#[from] redb::DatabaseError),
    #[error(transparent)]
    Transactions(#[from] redb::TransactionError),
    #[error(transparent)]
    Table(#[from] redb::TableError),
    #[error(transparent)]
    Storage(#[from] redb::StorageError),
    #[error(transparent)]
    Commit(#[from] redb::CommitError),
    #[error(transparent)]
    Durability(#[from] redb::SetDurabilityError),
    #[error(transparent)]
    Compaction(#[from] redb::CompactionError),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("backup size limit exceeded: {0} bytes")]
    SizeExceeded(u64),
    #[error("failed to join flushing task")]
    JoinTask,
}

/// A disk backup for failed commits. This handle to a database allows to write only to one table
/// for scoped access. If you want to write to another table, clone it using
/// [`Self::clone_with_table`].
#[derive(Debug, Clone)]
pub struct DiskBackup {
    db: Arc<RwLock<redb::Database>>,
    config: DiskBackupConfig,
}

impl DiskBackup {
    pub fn new(
        config: DiskBackupConfig,
        task_executor: &TaskExecutor,
    ) -> Result<Self, redb::DatabaseError> {
        // Ensure all parent directories exist, so that the database can be initialized correctly.
        if let Some(parent) = config.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let db = redb::Database::create(&config.path)?;

        let disk_backup = Self {
            db: Arc::new(RwLock::new(db)),
            config,
        };

        task_executor.spawn({
            let disk_backup: Self = disk_backup.clone();
            async move {
                disk_backup.flush_routine().await;
            }
        });

        Ok(disk_backup)
    }
}

impl DiskBackup {
    /// Saves a new failed commit to disk. `commit_immediately` indicates whether to force
    /// durability on write.
    fn save<T: ClickhouseRowExt>(
        &mut self,
        data: &FailedCommit<T>,
    ) -> Result<BackupSourceStats, DiskBackupError> {
        let table_def = Table::new(T::TABLE_NAME);
        // NOTE: not efficient, but we don't expect to store a lot of data here.
        let bytes = serde_json::to_vec(&data)?;

        let writer = self.db.write().expect("not poisoned").begin_write()?;
        let (stored_bytes, rows) = {
            let mut table = writer.open_table(table_def)?;
            if table.stats()?.stored_bytes() > self.config.max_size_bytes {
                return Err(DiskBackupError::SizeExceeded(self.config.max_size_bytes));
            }

            table.insert(new_disk_backup_key(), bytes)?;

            (table.stats()?.stored_bytes(), table.len()?)
        };
        writer.commit()?;

        Ok(BackupSourceStats {
            size_bytes: stored_bytes,
            total_batches: rows as usize,
        })
    }

    /// Retrieves the oldest failed commit from disk, if any.
    fn retrieve_oldest<T: ClickhouseRowExt>(
        &mut self,
    ) -> Result<Option<DiskRetrieval<DiskBackupKey, FailedCommit<T>>>, DiskBackupError> {
        let table_def = Table::new(T::TABLE_NAME);

        let reader = self.db.read().expect("not poisoned").begin_read()?;
        let table = match reader.open_table(table_def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                // No table means no data.
                return Ok(None);
            }
            Err(e) => {
                return Err(e.into());
            }
        };

        let stored_bytes = table.stats()?.stored_bytes();
        let rows = table.len()? as usize;
        let stats = BackupSourceStats {
            size_bytes: stored_bytes,
            total_batches: rows,
        };

        // Retreives in sorted order.
        let Some(entry_res) = table.iter()?.next() else {
            return Ok(None);
        };
        let (key, rows_raw) = entry_res?;
        let commit: FailedCommit<T> = serde_json::from_slice(&rows_raw.value())?;

        Ok(Some(DiskRetrieval {
            key: key.value(),
            value: commit,
            stats,
        }))
    }

    /// Deletes the failed commit with the given key from disk.
    fn delete<T: ClickhouseRowExt>(
        &mut self,
        key: DiskBackupKey,
    ) -> Result<BackupSourceStats, DiskBackupError> {
        let table_def = Table::new(T::TABLE_NAME);

        let mut writer = self.db.write().expect("not poisoned").begin_write()?;
        writer.set_durability(redb::Durability::Immediate)?;

        let (stored_bytes, rows) = {
            let mut table = writer.open_table(table_def)?;
            table.remove(key)?;
            (table.stats()?.stored_bytes(), table.len()?)
        };
        writer.commit()?;

        Ok(BackupSourceStats {
            size_bytes: stored_bytes,
            total_batches: rows as usize,
        })
    }

    /// Explicity flushes any pending writes to disk. This is async to avoid blocking the main
    /// thread.
    async fn flush(&mut self) -> Result<(), DiskBackupError> {
        let db = self.db.clone();

        // Since this can easily block by a second or two, send it to a blocking thread.
        tokio::task::spawn_blocking(move || {
            let mut db = db.write().expect("not poisoned");
            let mut writer = db.begin_write()?;

            // If there is no data to flush, don't do anything.
            if writer.stats()?.stored_bytes() == 0 {
                return Ok(());
            }

            writer.set_durability(redb::Durability::Immediate)?;
            writer.commit()?;

            db.compact()?;
            Ok(())
        })
        .await
        .map_err(|_| DiskBackupError::JoinTask)?
    }

    /// Takes an instance of self and performs a flush routine if the immediate flush interval has
    /// ticked.
    async fn flush_routine(mut self) {
        loop {
            self.config.flush_interval.tick().await;
            let start = Instant::now();
            match self.flush().await {
                Ok(_) => {
                    tracing::debug!(target: TARGET, elapsed = ?start.elapsed(), "flushed backup write buffer to disk");
                }
                Err(e) => {
                    tracing::error!(target: TARGET, ?e, "failed to flush backup write buffer to disk");
                }
            }
        }
    }
}

/// Statistics about the Clickhouse data stored in a certain backup source (disk or memory).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BackupSourceStats {
    /// The total size in bytes of failed commit batches stored.
    size_bytes: u64,
    /// The total number of failed commit batches stored.
    total_batches: usize,
}

/// An in-memory backup for failed commits.
#[derive(Deref, DerefMut)]
struct MemoryBackup<T> {
    /// The in-memory cache of failed commits.
    #[deref]
    #[deref_mut]
    failed_commits: FailedCommits<T>,
    /// The configuration for the in-memory backup.
    config: MemoryBackupConfig,
    /// The statistics about the in-memory backup.
    stats: BackupSourceStats,
}

impl<T> MemoryBackup<T> {
    /// Updates the internal statistics and returns them.
    fn update_stats(&mut self) -> BackupSourceStats {
        let quantities = self.failed_commits.quantities();
        let new_len = self.failed_commits.len();

        self.stats = BackupSourceStats {
            size_bytes: quantities.bytes,
            total_batches: new_len,
        };
        self.stats
    }

    /// Checks whether the threshold for maximum size has been exceeded.
    fn threshold_exceeded(&self) -> bool {
        self.stats.size_bytes > self.config.max_size_bytes && self.failed_commits.len() > 1
    }

    /// Drops the oldest failed commit if the threshold has been exceeded,
    /// returning (updated stats, Quantities of the dropped commit)
    fn drop_excess(&mut self) -> Option<(BackupSourceStats, Quantities)> {
        if self.threshold_exceeded() {
            let dropped_quantities = self
                .failed_commits
                .pop_back()
                .map(|commit| commit.quantities)
                .unwrap_or(Quantities::ZERO);
            Some((self.update_stats(), dropped_quantities))
        } else {
            None
        }
    }

    /// Saves a new failed commit into memory, updating the stats.
    fn save(&mut self, data: FailedCommit<T>) -> BackupSourceStats {
        self.failed_commits.push_front(data);
        self.update_stats()
    }

    /// Retrieves the oldest failed commit from memory, updating the stats.
    fn retrieve_oldest(&mut self) -> Option<FailedCommit<T>> {
        let oldest = self.failed_commits.pop_back();
        self.update_stats();
        oldest
    }
}

// Needed otherwise requires T: Default
impl<T> Default for MemoryBackup<T> {
    fn default() -> Self {
        Self {
            failed_commits: FailedCommits::default(),
            config: MemoryBackupConfig::default(),
            stats: BackupSourceStats::default(),
        }
    }
}

/// An backup actor for Clickhouse data. This actor receives [`FailedCommit`]s and saves them on
/// disk and in memory in case of failure of the former, and periodically tries to commit them back
/// again to Clickhouse. Since memory is finite, there is an upper bound on how much memory this
/// data structure holds. Once this has been hit, pressure applies, meaning that we try again a
/// certain failed commit for a finite number of times, and then we discard it to accomdate new
/// data.
pub struct Backup<T: ClickhouseRowExt, MetricsType: Metrics> {
    /// The receiver of failed commit attempts.
    ///
    /// Rationale for sending multiple rows instead of sending rows: the backup abstraction must
    /// periodically block to write data to the inserter and try to commit it to clickhouse. Each
    /// attempt results in doing the previous step. This could clog the channel which will receive
    /// individual rows, leading to potential row losses.
    ///
    /// By sending backup data less often, we give time gaps for these operation to be performed.
    rx: mpsc::Receiver<FailedCommit<T>>,
    /// The disk cache of failed commits.
    disk_backup: DiskBackup,
    /// The in-memory cache of failed commits.
    memory_backup: MemoryBackup<T>,
    /// A clickhouse inserter for committing again the data.
    inserter: Inserter<T>,
    /// The interval at which we try to backup data.
    interval: BackoffInterval,

    /// A failed commit retrieved from either disk or memory, waiting to be retried.
    last_cached: Option<RetrievedFailedCommit<T>>,

    /// Whether to use only the in-memory backup (for testing purposes).
    #[cfg(any(test, feature = "test-utils"))]
    use_only_memory_backup: bool,
    _metrics_phantom: std::marker::PhantomData<MetricsType>,
}

impl<T: ClickhouseRowExt, MetricsType: Metrics> Backup<T, MetricsType> {
    pub fn new(
        rx: mpsc::Receiver<FailedCommit<T>>,
        inserter: Inserter<T>,
        disk_backup: DiskBackup,
    ) -> Self {
        Self {
            rx,
            inserter,
            interval: Default::default(),
            memory_backup: MemoryBackup::default(),
            disk_backup,
            last_cached: None,
            #[cfg(any(test, feature = "test-utils"))]
            use_only_memory_backup: false,
            _metrics_phantom: std::marker::PhantomData,
        }
    }

    /// Override the default memory backup configuration.
    pub fn with_memory_backup_config(mut self, config: MemoryBackupConfig) -> Self {
        self.memory_backup.config = config;
        self
    }

    /// Backs up a failed commit, first trying to write to disk, then to memory.
    fn backup(&mut self, failed_commit: FailedCommit<T>) {
        let quantities = failed_commit.quantities;
        tracing::debug!(target: TARGET, order = T::TABLE_NAME, bytes = ?quantities.bytes, rows = ?quantities.rows, "backing up failed commit");

        #[cfg(any(test, feature = "test-utils"))]
        if self.use_only_memory_backup {
            self.memory_backup.save(failed_commit);
            self.last_cached = self
                .last_cached
                .take()
                .filter(|cached| cached.source != BackupSource::Memory);
            return;
        }

        let start = Instant::now();
        match self.disk_backup.save(&failed_commit) {
            Ok(stats) => {
                tracing::debug!(target: TARGET, order = T::TABLE_NAME, total_size = stats.size_bytes.format_bytes(), elapsed = ?start.elapsed(), "saved failed commit to disk");
                MetricsType::set_disk_backup_size(
                    stats.size_bytes,
                    stats.total_batches,
                    T::TABLE_NAME,
                );

                return;
            }
            Err(e) => {
                tracing::error!(target: TARGET, order = T::TABLE_NAME, ?e, "failed to write commit, trying in-memory");
                MetricsType::increment_backup_disk_errors(T::TABLE_NAME, e.as_ref());
            }
        };

        let stats = self.memory_backup.save(failed_commit);
        MetricsType::set_memory_backup_size(stats.size_bytes, stats.total_batches, T::TABLE_NAME);
        tracing::debug!(target: TARGET, order = T::TABLE_NAME, bytes = ?quantities.bytes, rows = ?quantities.rows, ?stats, "saved failed commit in-memory");

        if let Some((stats, dropped_quantities)) = self.memory_backup.drop_excess() {
            tracing::warn!(target: TARGET, order = T::TABLE_NAME, ?stats, "failed commits exceeded max memory backup size, dropping oldest");
            MetricsType::process_backup_data_lost_quantities(&dropped_quantities);
            // Clear the cached last commit if it was from memory and we just dropped it.
            self.last_cached = self
                .last_cached
                .take()
                .filter(|cached| cached.source != BackupSource::Memory);
        }
    }

    /// Retrieves the oldest failed commit, first trying from memory, then from disk.
    fn retrieve_oldest(&mut self) -> Option<RetrievedFailedCommit<T>> {
        if let Some(cached) = self.last_cached.take() {
            tracing::debug!(target: TARGET, order = T::TABLE_NAME, rows = cached.commit.rows.len(), "retrieved last cached failed commit");
            return Some(cached);
        }

        if let Some(commit) = self.memory_backup.retrieve_oldest() {
            tracing::debug!(target: TARGET, order = T::TABLE_NAME, rows = commit.rows.len(), "retrieved oldest failed commit from memory");
            return Some(RetrievedFailedCommit {
                source: BackupSource::Memory,
                commit,
            });
        }

        match self.disk_backup.retrieve_oldest() {
            Ok(maybe_commit) => {
                maybe_commit.inspect(|data| {
                    tracing::debug!(target: TARGET, order = T::TABLE_NAME, rows = data.stats.total_batches, "retrieved oldest failed commit from disk");
                })
                .map(|data| RetrievedFailedCommit {
                    source: BackupSource::Disk(data.key),
                    commit: data.value,
                })
            }
            Err(e) => {
                tracing::error!(target: TARGET, order = T::TABLE_NAME, ?e, "failed to retrieve oldest failed commit from disk");
                MetricsType::increment_backup_disk_errors(T::TABLE_NAME, e.as_ref());
                None
            }
        }
    }

    /// Populates the inserter with the rows from the given failed commit.
    async fn populate_inserter(&mut self, commit: &FailedCommit<T>) {
        for row in &commit.rows {
            let value_ref = T::to_row_ref(row);

            if let Err(e) = self.inserter.write(value_ref).await {
                MetricsType::increment_write_failures(e.to_string());
                tracing::error!(target: TARGET, order = T::TABLE_NAME, ?e, "failed to write to backup inserter");
                continue;
            }
        }
    }

    /// Purges a committed failed commit from disk, if applicable.
    async fn purge_commit(&mut self, retrieved: &RetrievedFailedCommit<T>) {
        if let BackupSource::Disk(key) = retrieved.source {
            let start = Instant::now();
            match self.disk_backup.delete::<T>(key) {
                Ok(stats) => {
                    tracing::debug!(target: TARGET, order = T::TABLE_NAME, total_size = stats.size_bytes.format_bytes(), elapsed = ?start.elapsed(), "deleted failed commit from disk");
                    MetricsType::set_disk_backup_size(
                        stats.size_bytes,
                        stats.total_batches,
                        T::TABLE_NAME,
                    );
                }
                Err(e) => {
                    tracing::error!(target: TARGET, order = T::TABLE_NAME, ?e, "failed to purge failed commit from disk");
                }
            }
            tracing::debug!(target: TARGET, order = T::TABLE_NAME, "purged committed failed commit from disk");
        }
    }

    /// Run the backup actor until it is possible to receive messages.
    ///
    /// If some data were stored on disk previously, they will be retried first.
    async fn run(&mut self) {
        loop {
            tokio::select! {
                maybe_failed_commit = self.rx.recv() => {
                    let Some(failed_commit) = maybe_failed_commit else {
                        tracing::error!(target: TARGET, order = T::TABLE_NAME, "Backup channel closed");
                        break;
                    };

                    self.backup(failed_commit);
                }
                _ = self.interval.tick() => {
                    let Some(oldest) = self.retrieve_oldest() else {
                        self.interval.reset();
                        MetricsType::set_backup_empty_size(T::TABLE_NAME);
                        continue // Nothing to do!
                    };

                    self.populate_inserter(&oldest.commit).await;

                    let start = Instant::now();
                    match self.inserter.force_commit().await {
                        Ok(quantities) => {
                            tracing::info!(target: TARGET, order = T::TABLE_NAME, ?quantities, "successfully backed up");
                            MetricsType::process_backup_data_quantities(&quantities.into());
                            MetricsType::record_batch_commit_time(start.elapsed());
                            self.interval.reset();
                            self.purge_commit(&oldest).await;
                        }
                        Err(e) => {
                            tracing::error!(target: TARGET, order = T::TABLE_NAME, ?e, quantities = ?oldest.commit.quantities, "failed to commit bundle to clickhouse from backup");
                            MetricsType::increment_commit_failures(e.to_string());
                            self.last_cached = Some(oldest);
                            continue;
                        }
                    }
                }
            }
        }
    }

    /// To call on shutdown, tries make a last-resort attempt to post back to Clickhouse all
    /// in-memory data.
    async fn end(mut self) {
        for failed_commit in self.memory_backup.failed_commits.drain(..) {
            for row in &failed_commit.rows {
                let value_ref = T::to_row_ref(row);

                if let Err(e) = self.inserter.write(value_ref).await {
                    tracing::error!( target: TARGET, order = T::TABLE_NAME, ?e, "failed to write to backup inserter during shutdown");
                    MetricsType::increment_write_failures(e.to_string());
                    continue;
                }
            }
            if let Err(e) = self.inserter.force_commit().await {
                tracing::error!(target: TARGET, order = T::TABLE_NAME, ?e, "failed to commit backup to CH during shutdown, trying disk");
                MetricsType::increment_commit_failures(e.to_string());
            }

            if let Err(e) = self.disk_backup.save(&failed_commit) {
                tracing::error!(target: TARGET, order = T::TABLE_NAME, ?e, "failed to write commit to disk backup during shutdown");
                MetricsType::increment_backup_disk_errors(T::TABLE_NAME, e.as_ref());
            }
        }

        if let Err(e) = self.disk_backup.flush().await {
            tracing::error!(target: TARGET, order = T::TABLE_NAME, ?e, "Failed to flush disk backup during shutdown");
            MetricsType::increment_backup_disk_errors(T::TABLE_NAME, e.as_ref());
        } else {
            tracing::info!(target: TARGET, order = T::TABLE_NAME, "Flushed disk backup during shutdown");
        }

        if let Err(e) = self.inserter.end().await {
            tracing::error!(target: TARGET, order = T::TABLE_NAME, ?e, "Failed to end backup inserter during shutdown");
        } else {
            tracing::info!(target: TARGET, order = T::TABLE_NAME, "Successfully ended backup inserter during shutdown");
        }
    }

    /// Spawns the inserter runner on the given task executor.
    pub fn spawn(mut self, task_executor: &TaskExecutor, name: String, target: &'static str)
    where
        MetricsType: Send + Sync + 'static,
        for<'a> <T as clickhouse::Row>::Value<'a>: Sync,
    {
        task_executor.spawn_with_graceful_shutdown_signal(|shutdown| async move {
            let mut shutdown_guard = None;
            tokio::select! {
                _ = self.run() => {
                    tracing::info!(target,table_name = name, "Clickhouse backup channel closed");
                }
                guard = shutdown => {
                    tracing::info!(target, table_name = name,"Received shutdown backup, performing clickhouse backup cleanup");
                    shutdown_guard = Some(guard);
                },
            }
            self.end().await;
            tracing::info!(
                target,
                table_name = name,
                "Clickhouse backup cleanup complete"
            );
            drop(shutdown_guard);
        });
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl<T: ClickhouseRowExt, MetricsType: Metrics> Backup<T, MetricsType> {
    pub fn new_test(
        rx: mpsc::Receiver<FailedCommit<T>>,
        inserter: Inserter<T>,
        disk_backup: DiskBackup,
        use_only_memory_backup: bool,
    ) -> Self {
        use std::marker::PhantomData;

        Self {
            rx,
            inserter,
            interval: Default::default(),
            memory_backup: MemoryBackup::default(),
            disk_backup,
            last_cached: None,
            use_only_memory_backup,
            _metrics_phantom: PhantomData,
        }
    }
}
