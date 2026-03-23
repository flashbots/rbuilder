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
use tokio::task::JoinHandle;

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

/// Maximum number of rows to merge into a single backup commit. Matches the
/// default inserter's `max_rows` to keep request sizes bounded.
const MAX_ROWS_PER_BACKUP_COMMIT: usize = 65_536;

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

/// A [`FailedCommit`] along with its source(s) (disk or memory).
///
/// A single retrieval has one source, but merged batches can have multiple
/// sources (e.g. several disk keys + memory entries). Preserving all sources
/// is necessary so that disk entries can be purged after a successful commit,
/// even if the batch was previously re-stored after a failed attempt.
struct RetrievedFailedCommit<T> {
    sources: Vec<BackupSource>,
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

impl DiskBackupError {
    /// The error is related to some physical or logical disk problem.
    pub fn is_disk_error(&self) -> bool {
        matches!(
            self,
            Self::Database(_)
                | Self::Transactions(_)
                | Self::Table(_)
                | Self::Storage(_)
                | Self::Commit(_)
                | Self::Durability(_)
                | Self::Serde(_)
                | Self::Compaction(_)
        )
    }
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

    /// Saves a new failed commit to disk. `commit_immediately` indicates whether to force
    /// durability on write.
    fn get_table_stats<T: ClickhouseRowExt>(&self) -> Result<BackupSourceStats, DiskBackupError> {
        let table_def = Table::new(T::TABLE_NAME);
        let reader = self.db.read().expect("not poisoned").begin_read()?;
        let table = reader.open_table(table_def)?;
        Self::table_stats(&table)
    }
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
        let stats = {
            let mut table = writer.open_table(table_def)?;
            if table.stats()?.stored_bytes() > self.config.max_size_bytes {
                return Err(DiskBackupError::SizeExceeded(self.config.max_size_bytes));
            }

            table.insert(new_disk_backup_key(), bytes)?;

            Self::table_stats(&table)?
        };
        writer.commit()?;

        Ok(stats)
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

        let stats = Self::table_stats(&table)?;

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

        let stats = {
            let mut table = writer.open_table(table_def)?;
            table.remove(key)?;
            Self::table_stats(&table)?
        };
        writer.commit()?;

        Ok(stats)
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

    /// Extracts backup statistics from an open table (read or write).
    fn table_stats<T: redb::ReadableTable<DiskBackupKey, Vec<u8>>>(
        table: &T,
    ) -> Result<BackupSourceStats, DiskBackupError> {
        let stored_bytes = table.stats()?.stored_bytes();
        let rows = table.len()? as usize;
        Ok(BackupSourceStats {
            size_bytes: stored_bytes,
            total_batches: rows,
        })
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

    /// Maximum number of rows to merge into a single backup commit.
    max_rows_per_commit: usize,

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
        if let Ok(stats) = disk_backup.get_table_stats::<T>() {
            Self::update_disk_backup_stats(stats);
        } else {
            tracing::error!(target: TARGET, order = T::TABLE_NAME, "Failed to get initial disk backup stats");
        }
        Self {
            rx,
            inserter,
            interval: Default::default(),
            memory_backup: MemoryBackup::default(),
            disk_backup,
            last_cached: None,
            max_rows_per_commit: MAX_ROWS_PER_BACKUP_COMMIT,
            #[cfg(any(test, feature = "test-utils"))]
            use_only_memory_backup: false,
            _metrics_phantom: std::marker::PhantomData,
        }
    }

    fn update_disk_backup_stats(stats: BackupSourceStats) {
        MetricsType::set_disk_backup_size(stats.size_bytes, stats.total_batches, T::TABLE_NAME);
    }

    /// Override the default memory backup configuration.
    pub fn with_memory_backup_config(mut self, config: MemoryBackupConfig) -> Self {
        self.memory_backup.config = config;
        self
    }

    /// Helper to log disk backup errors and increment metrics.
    fn log_disk_error(message: &str, error: &DiskBackupError) {
        tracing::error!(target: TARGET, order = T::TABLE_NAME, ?error, message);
        if error.is_disk_error() {
            MetricsType::increment_backup_disk_errors(T::TABLE_NAME, error.as_ref());
        }
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
                .filter(|cached| cached.sources.iter().any(|s| *s != BackupSource::Memory));
            return;
        }

        let start = Instant::now();
        match self.disk_backup.save(&failed_commit) {
            Ok(stats) => {
                tracing::debug!(target: TARGET, order = T::TABLE_NAME, total_size = stats.size_bytes.format_bytes(), elapsed = ?start.elapsed(), "saved failed commit to disk");
                Self::update_disk_backup_stats(stats);
                return;
            }
            Err(e) => {
                Self::log_disk_error("failed to write commit, trying in-memory", &e);
            }
        };

        let stats = self.memory_backup.save(failed_commit);
        MetricsType::set_memory_backup_size(stats.size_bytes, stats.total_batches, T::TABLE_NAME);
        tracing::debug!(target: TARGET, order = T::TABLE_NAME, bytes = ?quantities.bytes, rows = ?quantities.rows, ?stats, "saved failed commit in-memory");

        if let Some((stats, dropped_quantities)) = self.memory_backup.drop_excess() {
            tracing::error!(target: TARGET, order = T::TABLE_NAME, ?stats, ?dropped_quantities, "failed commits exceeded max memory backup size, dropping oldest");
            MetricsType::process_backup_data_lost_quantities(&dropped_quantities);
            // Clear the cached last commit if it was from memory and we just dropped it.
            self.last_cached = self
                .last_cached
                .take()
                .filter(|cached| cached.sources.iter().any(|s| *s != BackupSource::Memory));
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
                sources: vec![BackupSource::Memory],
                commit,
            });
        }

        match self.disk_backup.retrieve_oldest() {
            Ok(maybe_commit) => {
                maybe_commit.inspect(|data| {
                    tracing::debug!(target: TARGET, order = T::TABLE_NAME, rows = data.stats.total_batches, "retrieved oldest failed commit from disk");
                })
                .map(|data| RetrievedFailedCommit {
                    sources: vec![BackupSource::Disk(data.key)],
                    commit: data.value,
                })
            }
            Err(e) => {
                Self::log_disk_error("failed to retrieve oldest failed commit from disk", &e);
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
    async fn purge_disk_backup(&mut self, source: &BackupSource) {
        if let BackupSource::Disk(key) = source {
            let key = *key;
            let start = Instant::now();
            match self.disk_backup.delete::<T>(key) {
                Ok(stats) => {
                    tracing::debug!(target: TARGET, order = T::TABLE_NAME, total_size = stats.size_bytes.format_bytes(), elapsed = ?start.elapsed(), "deleted failed commit from disk");
                    Self::update_disk_backup_stats(stats);
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
                    // Drain all backed-up batches by merging them into commits
                    // of up to MAX_ROWS_PER_BACKUP_COMMIT rows each. This
                    // balances fewer HTTP round-trips against bounded request
                    // sizes.
                    loop {
                        let mut sources: Vec<BackupSource> = Vec::new();
                        let mut merged_rows: Vec<T> = Vec::new();
                        let mut merged_quantities = Quantities::ZERO;

                        while merged_rows.len() < self.max_rows_per_commit {
                            let Some(oldest) = self.retrieve_oldest() else {
                                break;
                            };
                            merged_quantities.bytes += oldest.commit.quantities.bytes;
                            merged_quantities.rows += oldest.commit.quantities.rows;
                            merged_quantities.transactions += oldest.commit.quantities.transactions;
                            merged_rows.extend(oldest.commit.rows);
                            sources.extend(oldest.sources);
                        }

                        if sources.is_empty() {
                            self.interval.reset();
                            MetricsType::set_backup_empty_size(T::TABLE_NAME);
                            break;
                        }

                        let merged_commit = FailedCommit {
                            rows: merged_rows,
                            quantities: merged_quantities,
                        };

                        self.populate_inserter(&merged_commit).await;

                        let start = Instant::now();
                        match self.inserter.force_commit().await {
                            Ok(quantities) => {
                                let batch_count = sources.len();
                                tracing::info!(target: TARGET, order = T::TABLE_NAME, ?quantities, batch_count, "successfully backed up merged batches");
                                MetricsType::process_backup_data_quantities(&quantities.into());
                                MetricsType::record_batch_commit_time(start.elapsed());
                                for source in &sources {
                                    self.purge_disk_backup(source).await;
                                }
                                // Continue to drain more if there are remaining batches.
                            }
                            Err(e) => {
                                tracing::error!(target: TARGET, order = T::TABLE_NAME, ?e, ?merged_quantities, "failed to commit merged backup to clickhouse");
                                MetricsType::increment_commit_failures(e.to_string());
                                // Re-store the merged batch with its original
                                // sources so disk keys can be purged on eventual
                                // success.
                                self.last_cached = Some(RetrievedFailedCommit {
                                    sources,
                                    commit: merged_commit,
                                });
                                break;
                            }
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
                Self::log_disk_error("failed to write commit to disk backup during shutdown", &e);
            }
        }

        if let Err(e) = self.disk_backup.flush().await {
            Self::log_disk_error("Failed to flush disk backup during shutdown", &e);
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
    /// Returns a JoinHandle that resolves when the task completes.
    /// On shutdown will stop processing new data flush the backup. New data might be lost.
    pub fn spawn(
        mut self,
        task_executor: &TaskExecutor,
        name: String,
        target: &'static str,
    ) -> JoinHandle<()>
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
        })
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
            max_rows_per_commit: MAX_ROWS_PER_BACKUP_COMMIT,
            use_only_memory_backup,
            _metrics_phantom: PhantomData,
        }
    }

    pub fn with_max_rows_per_commit(mut self, max_rows: usize) -> Self {
        self.max_rows_per_commit = max_rows;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics::NullMetrics;
    use std::io::{Read as IoRead, Write as IoWrite};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    /// A minimal row type for testing.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, clickhouse::Row)]
    struct TestRow {
        value: u64,
    }

    impl ClickhouseRowExt for TestRow {
        type TraceId = u64;
        const TABLE_NAME: &'static str = "test_rows";

        fn trace_id(&self) -> u64 {
            self.value
        }

        fn to_row_ref(row: &Self) -> &<Self as clickhouse::Row>::Value<'_> {
            row
        }
    }

    /// Creates a DiskBackup without a TaskExecutor (no background flush routine).
    fn test_disk_backup() -> DiskBackup {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test_backup.db");
        let db = redb::Database::create(&path).unwrap();
        // Leak the tempdir so it stays alive for the test duration.
        std::mem::forget(tmp);
        DiskBackup {
            db: Arc::new(RwLock::new(db)),
            config: DiskBackupConfig::new(),
        }
    }

    fn make_failed_commit(n_rows: usize) -> FailedCommit<TestRow> {
        let rows: Vec<TestRow> = (0..n_rows).map(|i| TestRow { value: i as u64 }).collect();
        FailedCommit {
            rows,
            quantities: Quantities {
                bytes: n_rows as u64,
                rows: n_rows as u64,
                transactions: 1,
            },
        }
    }

    /// Starts a mock ClickHouse HTTP server.
    ///
    /// `max_successes`: how many successful (200) responses to return before
    /// shutting down the server (causing connection refused). If `None`, always
    /// succeeds.
    ///
    /// Returns the (address, request_count).
    fn start_mock_clickhouse(
        max_successes: Option<usize>,
    ) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let request_count = Arc::new(AtomicUsize::new(0));
        let count = request_count.clone();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0u8; 65536];
                let _ = stream.read(&mut buf);

                let n = count.fetch_add(1, Ordering::SeqCst);
                if max_successes.is_none_or(|max| n < max) {
                    let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                } else {
                    // Drop the listener to cause "connection refused" for
                    // subsequent attempts. Drop the stream without responding.
                    drop(stream);
                    drop(listener);
                    return;
                }
            }
        });

        (addr, request_count)
    }

    fn make_backup(
        addr: std::net::SocketAddr,
        rx: mpsc::Receiver<FailedCommit<TestRow>>,
    ) -> Backup<TestRow, NullMetrics> {
        let client = clickhouse::Client::default().with_url(format!("http://{}", addr));
        let inserter = client
            .inserter::<TestRow>("test_rows")
            .with_period(Some(Duration::from_secs(60)))
            .with_timeouts(Some(Duration::from_secs(5)), Some(Duration::from_secs(5)));
        Backup::<TestRow, NullMetrics>::new_test(rx, inserter, test_disk_backup(), true)
    }

    /// After accumulating several batches in the memory backup, the drain loop
    /// should commit all of them in a single tick cycle without returning to
    /// `select!` between each one.
    #[tokio::test]
    async fn drain_loop_commits_all_batches_on_success() {
        let (addr, _request_count) = start_mock_clickhouse(None);
        let (tx, rx) = mpsc::channel::<FailedCommit<TestRow>>(64);
        let mut backup = make_backup(addr, rx);

        // Directly populate the memory backup with 5 batches.
        let num_batches = 5usize;
        for _ in 0..num_batches {
            backup.memory_backup.save(make_failed_commit(3));
        }
        assert_eq!(backup.memory_backup.failed_commits.len(), num_batches);

        // Keep tx alive so rx.recv() blocks (doesn't return None).
        // run() will:
        //   1. tick fires immediately (BackoffInterval starts at now)
        //   2. inner drain loop commits all 5 batches
        //   3. returns to select!, blocks on rx.recv() + next tick
        let handle = tokio::spawn(async move {
            backup.run().await;
            backup // return backup so we can inspect it
        });

        // Give enough real time for the drain to complete (network I/O with mock).
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Drop tx to unblock run() and let it exit.
        drop(tx);
        let backup = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("run() should exit after channel close")
            .unwrap();

        // All batches should have been drained from memory.
        assert_eq!(
            backup.memory_backup.failed_commits.len(),
            0,
            "all backed-up batches should be drained from memory"
        );
        assert!(
            backup.last_cached.is_none(),
            "no failed batch should be cached after successful drain"
        );
    }

    /// `retrieve_oldest` returns `last_cached` first, then pops from memory,
    /// and returns `None` when both are empty.
    #[tokio::test]
    async fn retrieve_oldest_prioritizes_cached_over_memory() {
        let (addr, _) = start_mock_clickhouse(None);
        let (_tx, rx) = mpsc::channel::<FailedCommit<TestRow>>(64);
        let mut backup = make_backup(addr, rx);

        // Populate memory with 2 batches.
        backup.memory_backup.save(make_failed_commit(1));
        backup.memory_backup.save(make_failed_commit(2));

        // Simulate a cached failed commit (as if force_commit had failed).
        let cached_commit = make_failed_commit(99);
        backup.last_cached = Some(RetrievedFailedCommit {
            sources: vec![BackupSource::Memory],
            commit: cached_commit,
        });

        // First retrieval should return the cached one.
        let first = backup.retrieve_oldest().unwrap();
        assert_eq!(first.commit.rows.len(), 99);
        assert!(backup.last_cached.is_none());

        // Next retrievals come from memory (oldest first = the one with 1 row).
        let second = backup.retrieve_oldest().unwrap();
        assert_eq!(second.commit.rows.len(), 1);

        let third = backup.retrieve_oldest().unwrap();
        assert_eq!(third.commit.rows.len(), 2);

        // No more batches.
        assert!(backup.retrieve_oldest().is_none());
    }

    /// When a failed commit is received via the channel, it is stored in
    /// memory backup and can be retrieved later for draining.
    #[tokio::test]
    async fn backup_stores_failed_commits_in_memory() {
        let (addr, _) = start_mock_clickhouse(None);
        let (tx, rx) = mpsc::channel::<FailedCommit<TestRow>>(64);
        let mut backup = make_backup(addr, rx);

        let commit = make_failed_commit(7);
        backup.backup(commit);

        assert_eq!(backup.memory_backup.failed_commits.len(), 1);

        let retrieved = backup.retrieve_oldest().unwrap();
        assert_eq!(retrieved.commit.rows.len(), 7);
        assert_eq!(retrieved.sources, vec![BackupSource::Memory]);

        drop(tx);
    }

    /// Helper that runs the drain loop and returns the backup for inspection.
    async fn run_drain(mut backup: Backup<TestRow, NullMetrics>, tx: mpsc::Sender<FailedCommit<TestRow>>) -> Backup<TestRow, NullMetrics> {
        let handle = tokio::spawn(async move {
            backup.run().await;
            backup
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        drop(tx);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("run() should exit after channel close")
            .unwrap()
    }

    /// When total rows across batches exceed `max_rows_per_commit`, the drain
    /// loop splits them into multiple commits.
    #[tokio::test]
    async fn drain_loop_splits_batches_exceeding_row_limit() {
        let (addr, _) = start_mock_clickhouse(None);
        let (tx, rx) = mpsc::channel::<FailedCommit<TestRow>>(64);
        let mut backup = make_backup(addr, rx).with_max_rows_per_commit(10);

        // 5 batches of 4 rows = 20 rows total, limit is 10.
        // Gather while merged < 10:
        //   Commit 1: batch(4)→4, batch(4)→8, batch(4)→12 >= 10, stop → commit 12
        //   Commit 2: batch(4)→4, batch(4)→8, no more → commit 8
        // Total: 2 commits.
        for _ in 0..5 {
            backup.memory_backup.save(make_failed_commit(4));
        }

        let backup = run_drain(backup, tx).await;

        assert_eq!(backup.memory_backup.failed_commits.len(), 0);
        assert!(backup.last_cached.is_none());
    }

    /// When a single batch is larger than `max_rows_per_commit`, it is still
    /// processed (the limit governs when to stop *gathering*, not a hard cap
    /// on commit size).
    #[tokio::test]
    async fn drain_loop_handles_single_batch_exceeding_limit() {
        let (addr, _) = start_mock_clickhouse(None);
        let (tx, rx) = mpsc::channel::<FailedCommit<TestRow>>(64);
        let mut backup = make_backup(addr, rx).with_max_rows_per_commit(5);

        // One batch of 20 rows, limit is 5. The gather loop adds it (since
        // merged is initially 0 < 5), then stops. Committed in one go.
        backup.memory_backup.save(make_failed_commit(20));

        let backup = run_drain(backup, tx).await;

        assert_eq!(backup.memory_backup.failed_commits.len(), 0);
        assert!(backup.last_cached.is_none());
    }

    /// With a row limit of 1, each batch gets its own commit (no merging).
    #[tokio::test]
    async fn drain_loop_no_merging_with_limit_of_one() {
        let (addr, _) = start_mock_clickhouse(None);
        let (tx, rx) = mpsc::channel::<FailedCommit<TestRow>>(64);
        let mut backup = make_backup(addr, rx).with_max_rows_per_commit(1);

        // 3 batches of 2 rows each. With limit=1, each batch is gathered
        // individually (0 < 1 -> add batch -> 2 >= 1 -> stop -> commit).
        for _ in 0..3 {
            backup.memory_backup.save(make_failed_commit(2));
        }

        let backup = run_drain(backup, tx).await;

        assert_eq!(backup.memory_backup.failed_commits.len(), 0);
        assert!(backup.last_cached.is_none());
    }

    /// Batches that exactly hit the row limit are committed without merging
    /// additional batches.
    #[tokio::test]
    async fn drain_loop_exact_limit_boundary() {
        let (addr, _) = start_mock_clickhouse(None);
        let (tx, rx) = mpsc::channel::<FailedCommit<TestRow>>(64);
        let mut backup = make_backup(addr, rx).with_max_rows_per_commit(10);

        // 3 batches: 10, 10, 5 rows. Each 10-row batch fills the limit
        // exactly: gather batch(10) -> 10 >= 10 -> commit. Then batch(10)
        // again. Then batch(5). Total: 3 commits.
        backup.memory_backup.save(make_failed_commit(10));
        backup.memory_backup.save(make_failed_commit(10));
        backup.memory_backup.save(make_failed_commit(5));

        let backup = run_drain(backup, tx).await;

        assert_eq!(backup.memory_backup.failed_commits.len(), 0);
        assert!(backup.last_cached.is_none());
    }

    // --- Bug regression tests ---

    /// Regression: When a merged commit fails, the original sources (including
    /// disk keys) must be preserved in `last_cached` so they can be purged
    /// after eventual success. Previously they were replaced with
    /// `BackupSource::Memory`, causing disk entries to be orphaned and later
    /// double-inserted.
    #[tokio::test]
    async fn failed_merge_preserves_disk_source_keys() {
        let (addr, _) = start_mock_clickhouse(None);
        let (_tx, rx) = mpsc::channel::<FailedCommit<TestRow>>(64);
        let mut backup = make_backup(addr, rx);

        let disk_key_1: DiskBackupKey = 1001;
        let disk_key_2: DiskBackupKey = 1002;

        // Simulate what the drain loop does on failure: store merged batch
        // with its original sources.
        backup.last_cached = Some(RetrievedFailedCommit {
            sources: vec![
                BackupSource::Disk(disk_key_1),
                BackupSource::Memory,
                BackupSource::Disk(disk_key_2),
            ],
            commit: make_failed_commit(10),
        });

        let retrieved = backup.retrieve_oldest().unwrap();

        // Disk keys must be preserved so purge_disk_backup can delete them.
        let disk_keys: Vec<_> = retrieved
            .sources
            .iter()
            .filter_map(|s| match s {
                BackupSource::Disk(k) => Some(*k),
                _ => None,
            })
            .collect();
        assert_eq!(disk_keys, vec![disk_key_1, disk_key_2]);
    }

    // --- Integration tests (require a real ClickHouse at localhost:8123) ---

    /// End-to-end test: accumulate N batches in the backup, let the drain loop
    /// flush them to a real ClickHouse instance, and verify all rows arrive
    /// exactly once (no duplicates, no data loss).
    #[tokio::test]
    #[ignore = "requires ClickHouse at localhost:8123 — run via scripts/test-clickhouse-backup-drain.sh"]
    async fn integration_drain_to_real_clickhouse() {
        let client = clickhouse::Client::default()
            .with_url("http://localhost:8123")
            .with_database("default");

        // Create the test table (idempotent).
        client
            .query("CREATE TABLE IF NOT EXISTS test_rows (value UInt64) ENGINE = MergeTree() ORDER BY value")
            .execute()
            .await
            .expect("failed to create test table");

        // Truncate to start clean.
        client
            .query("TRUNCATE TABLE test_rows")
            .execute()
            .await
            .expect("failed to truncate test table");

        let inserter = client
            .inserter::<TestRow>("test_rows")
            .with_period(Some(Duration::from_secs(60)))
            .with_timeouts(Some(Duration::from_secs(5)), Some(Duration::from_secs(5)));

        let (tx, rx) = mpsc::channel::<FailedCommit<TestRow>>(64);
        let mut backup =
            Backup::<TestRow, NullMetrics>::new_test(rx, inserter, test_disk_backup(), true)
                .with_max_rows_per_commit(50);

        // Accumulate 10 batches of 10 rows each = 100 rows total.
        // Use unique values so we can detect duplicates.
        let total_rows = 100usize;
        let batch_size = 10usize;
        for batch_idx in 0..(total_rows / batch_size) {
            let rows: Vec<TestRow> = (0..batch_size)
                .map(|i| TestRow {
                    value: (batch_idx * batch_size + i) as u64,
                })
                .collect();
            backup.memory_backup.save(FailedCommit {
                rows,
                quantities: Quantities {
                    bytes: batch_size as u64,
                    rows: batch_size as u64,
                    transactions: 1,
                },
            });
        }

        // Run the drain loop until all batches are flushed.
        let handle = tokio::spawn(async move {
            backup.run().await;
            backup
        });

        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(tx);
        let backup = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("run() should complete")
            .unwrap();

        assert_eq!(backup.memory_backup.failed_commits.len(), 0, "all batches should be drained");
        assert!(backup.last_cached.is_none(), "nothing should be cached");

        // Give ClickHouse a moment to process async inserts.
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Verify row count.
        let row_count: u64 = client
            .query("SELECT count() FROM test_rows")
            .fetch_one()
            .await
            .expect("failed to query row count");
        assert_eq!(row_count, total_rows as u64, "all rows should be in ClickHouse");

        // Verify no duplicates.
        let dup_count: u64 = client
            .query("SELECT count() FROM (SELECT value, count() as cnt FROM test_rows GROUP BY value HAVING cnt > 1)")
            .fetch_one()
            .await
            .expect("failed to query duplicates");
        assert_eq!(dup_count, 0, "there should be no duplicate rows");
    }

    /// End-to-end test: simulate a ClickHouse outage mid-drain by stopping the
    /// Docker container, accumulate batches, restart, and verify everything
    /// drains without duplicates.
    #[tokio::test]
    #[ignore = "requires ClickHouse at localhost:8123 — run via scripts/test-clickhouse-backup-drain.sh"]
    async fn integration_drain_survives_outage() {
        let client = clickhouse::Client::default()
            .with_url("http://localhost:8123")
            .with_database("default");

        client
            .query("CREATE TABLE IF NOT EXISTS test_rows (value UInt64) ENGINE = MergeTree() ORDER BY value")
            .execute()
            .await
            .expect("failed to create test table");
        client
            .query("TRUNCATE TABLE test_rows")
            .execute()
            .await
            .expect("failed to truncate test table");

        let inserter = client
            .inserter::<TestRow>("test_rows")
            .with_period(Some(Duration::from_secs(60)))
            .with_timeouts(Some(Duration::from_secs(2)), Some(Duration::from_secs(2)));

        let (tx, rx) = mpsc::channel::<FailedCommit<TestRow>>(128);
        let mut backup =
            Backup::<TestRow, NullMetrics>::new_test(rx, inserter, test_disk_backup(), true)
                .with_max_rows_per_commit(30);

        // Phase 1: Insert 50 rows while ClickHouse is healthy.
        for i in 0..5 {
            let rows: Vec<TestRow> = (0..10)
                .map(|j| TestRow { value: (i * 10 + j) as u64 })
                .collect();
            backup.memory_backup.save(FailedCommit {
                rows,
                quantities: Quantities { bytes: 10, rows: 10, transactions: 1 },
            });
        }

        // Phase 2: Stop ClickHouse.
        let docker_stop = std::process::Command::new("docker")
            .args(["stop", "rbuilder-ch-test"])
            .output()
            .expect("failed to run docker stop");
        assert!(docker_stop.status.success(), "docker stop failed");

        // Add 50 more rows while ClickHouse is down.
        for i in 5..10 {
            let rows: Vec<TestRow> = (0..10)
                .map(|j| TestRow { value: (i * 10 + j) as u64 })
                .collect();
            backup.memory_backup.save(FailedCommit {
                rows,
                quantities: Quantities { bytes: 10, rows: 10, transactions: 1 },
            });
        }

        // Phase 3: Restart ClickHouse.
        let docker_start = std::process::Command::new("docker")
            .args(["start", "rbuilder-ch-test"])
            .output()
            .expect("failed to run docker start");
        assert!(docker_start.status.success(), "docker start failed");

        // Wait for ClickHouse to be ready.
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if client.query("SELECT 1").fetch_one::<u8>().await.is_ok() {
                break;
            }
        }

        // Run the drain loop.
        let handle = tokio::spawn(async move {
            backup.run().await;
            backup
        });

        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(tx);
        let backup = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("run() should complete")
            .unwrap();

        assert_eq!(backup.memory_backup.failed_commits.len(), 0);
        assert!(backup.last_cached.is_none());

        tokio::time::sleep(Duration::from_secs(1)).await;

        let row_count: u64 = client
            .query("SELECT count() FROM test_rows")
            .fetch_one()
            .await
            .expect("failed to query row count");
        assert_eq!(row_count, 100, "all 100 rows should be in ClickHouse");

        let dup_count: u64 = client
            .query("SELECT count() FROM (SELECT value, count() as cnt FROM test_rows GROUP BY value HAVING cnt > 1)")
            .fetch_one()
            .await
            .expect("failed to query duplicates");
        assert_eq!(dup_count, 0, "there should be no duplicate rows");
    }

    /// Regression: `last_cached` holding a merged batch with disk sources must
    /// survive memory pressure (`drop_excess`). Previously, when all sources
    /// were replaced with `BackupSource::Memory`, the merged batch would be
    /// silently dropped — permanent data loss.
    #[tokio::test]
    async fn last_cached_with_disk_sources_survives_memory_pressure() {
        let (addr, _) = start_mock_clickhouse(None);
        let (_tx, rx) = mpsc::channel::<FailedCommit<TestRow>>(64);
        let mut backup = make_backup(addr, rx);
        // Set a tiny memory limit so drop_excess fires easily.
        backup.memory_backup.config = MemoryBackupConfig::new(1);

        // Simulate a failed merged commit that contained disk-sourced data.
        backup.last_cached = Some(RetrievedFailedCommit {
            sources: vec![BackupSource::Disk(2001), BackupSource::Memory],
            commit: make_failed_commit(50),
        });

        // Add enough data to memory to trigger drop_excess.
        backup.memory_backup.save(make_failed_commit(100));
        backup.memory_backup.save(make_failed_commit(100));

        // Trigger the backup path which calls drop_excess.
        backup.backup(make_failed_commit(100));

        // last_cached should survive because it contains disk-sourced data.
        assert!(
            backup.last_cached.is_some(),
            "last_cached with disk sources should NOT be dropped under memory pressure"
        );
    }
}
