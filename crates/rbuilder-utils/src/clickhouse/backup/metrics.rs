use crate::clickhouse::Quantities;
use std::time::Duration;

/// Metrics updated by the clickhouse_with_backup mod.
pub trait Metrics {
    /// Failed to write the data to clickhouse either on the first try (ClickhouseInserter) or from backup failures, labelled with the error.
    fn increment_write_failures(err: String);
    /// We just inserted a batch of Quantities of data into clickhouse (by the ClickhouseInserter) on the first try, no backup involved.
    fn process_quantities(quantities: &Quantities);
    /// Time taken to commit the data to clickhouse either on the first try or from backup.
    fn record_batch_commit_time(duration: Duration);
    /// Failed to commit the data to clickhouse either on the first try or from backup, labelled with the error.
    fn increment_commit_failures(err: String);
    /// Size of the in-memory queue of the task that is inserting into clickhouse.
    fn set_queue_size(size: usize, order: &'static str);
    /// Space used by the local DB for failed commit batches.
    fn set_disk_backup_size(size_bytes: u64, batches: usize, order: &'static str);
    /// The total number of errors related to the disk backup, it can be reading, writing, etc.
    fn increment_backup_disk_errors(order: &'static str, error: &str);
    /// The size of the in-memory backup for failed commit batches.
    fn set_memory_backup_size(size_bytes: u64, batches: usize, order: &'static str);
    /// Some Quantities of data has been definitely lost from the backup DB and could not be committed to
    /// clickhouse.
    fn process_backup_data_lost_quantities(quantities: &Quantities);
    /// Some rows Quantities data was restored from the backup and committed to clickhouse.
    fn process_backup_data_quantities(quantities: &Quantities);
    /// Backup was emptied. No more unsaved data to commit. Equivalent to set_disk_backup_size(0,0,order)+set_memory_backup_size(0,0,order)
    fn set_backup_empty_size(order: &'static str) {
        Self::set_memory_backup_size(0, 0, order);
        Self::set_disk_backup_size(0, 0, order);
    }
}

/// Feeling lazy? Grafana is too expensive for you?
/// Use NullMetrics!
pub struct NullMetrics {}
impl Metrics for NullMetrics {
    fn increment_write_failures(_err: String) {
        // No-op
    }

    fn process_quantities(_quantities: &Quantities) {
        // No-op
    }

    fn record_batch_commit_time(_duration: Duration) {
        // No-op
    }

    fn increment_commit_failures(_err: String) {
        // No-op
    }

    fn set_queue_size(_size: usize, _order: &'static str) {
        // No-op
    }

    fn set_disk_backup_size(_size_bytes: u64, _batches: usize, _order: &'static str) {
        // No-op
    }

    fn increment_backup_disk_errors(_order: &'static str, _error: &str) {
        // No-op
    }

    fn set_memory_backup_size(_size_bytes: u64, _batches: usize, _order: &'static str) {
        // No-op
    }

    fn process_backup_data_lost_quantities(_quantities: &Quantities) {
        // No-op
    }

    fn process_backup_data_quantities(_quantities: &Quantities) {
        // No-op
    }

    fn set_backup_empty_size(_order: &'static str) {
        // No-op
    }
}
