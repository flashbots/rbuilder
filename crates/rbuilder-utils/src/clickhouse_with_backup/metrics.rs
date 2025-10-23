use crate::clickhouse::Quantities;
use std::time::Duration;

/// Metrics updated by the clickhouse_with_backup mod.
pub trait Metrics {
    fn increment_write_failures(err: String);
    fn process_quantities(quantities: &Quantities);
    fn record_batch_commit_time(duration: Duration);
    fn increment_commit_failures(err: String);
    fn set_queue_size(size: usize, order: &'static str);
    fn set_disk_backup_size(size_bytes: u64, batches: usize, order: &'static str);
    fn increment_backup_disk_errors(order: &'static str, error: &str);
    fn set_memory_backup_size(size_bytes: u64, batches: usize, order: &'static str);
    fn process_backup_data_lost_quantities(quantities: &Quantities);
    fn process_backup_data_quantities(quantities: &Quantities);
    fn set_backup_empty_size(order: &'static str);
}
