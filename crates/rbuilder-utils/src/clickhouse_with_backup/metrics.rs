use crate::clickhouse::Quantities;
use std::time::Duration;

/// Metrics updated by the clickhouse_with_backup mod.
pub trait Metrics {
    fn increment_clickhouse_write_failures(err: String);
    fn process_clickhouse_quantities(quantities: &Quantities);
    fn record_clickhouse_batch_commit_time(duration: Duration);
    fn increment_clickhouse_commit_failures(err: String);
    fn set_clickhouse_queue_size(size: usize, order: &'static str);
    fn set_clickhouse_disk_backup_size(size_bytes: u64, batches: usize, order: &'static str);
    fn increment_clickhouse_backup_disk_errors(order: &'static str, error: &str);
    fn set_clickhouse_memory_backup_size(size_bytes: u64, batches: usize, order: &'static str);
    fn process_clickhouse_backup_data_lost_quantities(quantities: &Quantities);
}
