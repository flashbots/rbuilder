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
