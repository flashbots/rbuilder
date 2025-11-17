#![allow(unexpected_cfgs)]

use std::time::Duration;

use ctor::ctor;
use lazy_static::lazy_static;
use metrics_macros::register_metrics;
use prometheus::{
    HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
};
use rbuilder::{
    telemetry::{exponential_buckets_range, REGISTRY},
    utils::{self, build_info::Version},
};
use rbuilder_utils::clickhouse::Quantities;

register_metrics! {
    pub static BLOCK_API_ERRORS: IntCounterVec = IntCounterVec::new(
        Opts::new("block_api_errors", "counter of the block processor errors"),
        &["api_name"]
    )
    .unwrap();

    pub static BIDDING_SERVICE_VERSION: IntGaugeVec = IntGaugeVec::new(
        Opts::new("bidding_service_version", "Version of the bidding service"),
        &["git", "git_ref", "build_time_utc"]
    )
    .unwrap();

    pub static CLICKHOUSE_WRITE_FAILURES: IntCounter = IntCounter::new("clickhouse_write_failures", "Clickhouse write failures for built blocks")
    .unwrap();
    pub static CLICKHOUSE_ROWS_COMMITTED: IntCounter = IntCounter::new("clickhouse_rows_committed", "Clickhouse built blocks commited directly to clickhouse (no backup involved)")
    .unwrap();
    pub static CLICKHOUSE_BYTES_COMMITTED: IntCounter = IntCounter::new("clickhouse_bytes_committed", "Clickhouse built blocks bytes commited directly to clickhouse (no backup involved)")
    .unwrap();
    pub static CLICKHOUSE_BATCHES_COMMITTED: IntCounter = IntCounter::new("clickhouse_batches_committed", "Clickhouse built blocks batches commited directly to clickhouse (no backup involved)")
    .unwrap();
    pub static CLICKHOUSE_ROWS_COMMITTED_FROM_BACKUP: IntCounter = IntCounter::new("clickhouse_rows_committed_from_backup", "Clickhouse built blocks commited to clickhouse from the local backup")
    .unwrap();
    pub static CLICKHOUSE_BYTES_COMMITTED_FROM_BACKUP: IntCounter = IntCounter::new("clickhouse_bytes_committed_from_backup", "Clickhouse built blocks bytes commited to clickhouse from the local backup")
    .unwrap();

    pub static CLICKHOUSE_ROWS_LOST: IntCounter = IntCounter::new("clickhouse_rows_lost", "clickhouse_rows_lost")
    .unwrap();
    pub static CLICKHOUSE_BYTES_LOST: IntCounter = IntCounter::new("clickhouse_bytes_lost", "clickhouse_bytes_lost")
    .unwrap();


    pub static CLICKHOUSE_COMMIT_FAILURES: IntCounter = IntCounter::new("clickhouse_commit_failures", "Clickhouse built blocks batches commited failures")
    .unwrap();
    pub static CLICKHOUSE_BACKUP_DISK_ERRORS: IntCounter = IntCounter::new("clickhouse_backup_disk_errors", "Any problem related to the disk backup, it can be reading, writing, etc.")
    .unwrap();
    pub static CLICKHOUSE_BATCH_COMMIT_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("clickhouse_batch_commit_time","Time to commit a block batch to Clickhouse (ms)")
            .buckets(exponential_buckets_range(0.5, 3000.0, 50)),
        &[]
    )
    .unwrap();
    pub static CLICKHOUSE_QUEUE_SIZE: IntGauge =
        IntGauge::new("clickhouse_queue_size", "Size of the queue of the task that is inserting into clickhouse").unwrap();
    pub static CLICKHOUSE_DISK_BACKUP_SIZE_BYTES: IntGauge =
        IntGauge::new("clickhouse_disk_backup_size_bytes", "Space used in bytes by the local DB for failed commit batches.").unwrap();
    pub static CLICKHOUSE_DISK_BACKUP_SIZE_BATCHES: IntGauge =
        IntGauge::new("clickhouse_disk_backup_size_batches", "Amount of batches in local DB for failed commit batches.").unwrap();
    pub static CLICKHOUSE_MEMORY_BACKUP_SIZE_BYTES: IntGauge =
        IntGauge::new("clickhouse_memory_backup_size_bytes", "Space used in bytes by the in memory DB for failed commit batches.").unwrap();
    pub static CLICKHOUSE_MEMORY_BACKUP_SIZE_BATCHES: IntGauge =
        IntGauge::new("clickhouse_memory_backup_size_batches", "Amount of batches in in memory DB for failed commit batches.").unwrap();





}

/*
    /// Space used by the local DB for failed commit batches.
    fn set_disk_backup_size(size_bytes: u64, batches: usize, order: &'static str);
    fn increment_backup_disk_errors(order: &'static str, error: &str);
    /// Space used in memory for failed commit batches.
    fn set_memory_backup_size(size_bytes: u64, batches: usize, order: &'static str);

*/

pub fn inc_submit_block_errors() {
    BLOCK_API_ERRORS.with_label_values(&["submit_block"]).inc()
}

pub fn inc_publish_tbv_errors() {
    BLOCK_API_ERRORS.with_label_values(&["publish_tbv"]).inc()
}

pub(super) fn set_bidding_service_version(version: Version) {
    BIDDING_SERVICE_VERSION
        .with_label_values(&[
            &version.git_commit,
            &version.git_ref,
            &version.build_time_utc,
        ])
        .set(1);
}

pub(crate) struct ClickhouseMetrics {}

impl rbuilder_utils::clickhouse::backup::metrics::Metrics for ClickhouseMetrics {
    fn increment_write_failures(_err: String) {
        CLICKHOUSE_WRITE_FAILURES.inc();
    }

    fn process_quantities(quantities: &Quantities) {
        CLICKHOUSE_ROWS_COMMITTED.inc_by(quantities.rows);
        CLICKHOUSE_BYTES_COMMITTED.inc_by(quantities.bytes);
        CLICKHOUSE_BATCHES_COMMITTED.inc();
    }

    fn record_batch_commit_time(duration: Duration) {
        CLICKHOUSE_BATCH_COMMIT_TIME
            .with_label_values(&[])
            .observe(utils::duration_ms(duration));
    }

    fn increment_commit_failures(_err: String) {
        CLICKHOUSE_COMMIT_FAILURES.inc();
    }

    fn set_queue_size(size: usize, _order: &'static str) {
        CLICKHOUSE_QUEUE_SIZE.set(size as i64);
    }

    fn set_disk_backup_size(size_bytes: u64, batches: usize, _order: &'static str) {
        CLICKHOUSE_DISK_BACKUP_SIZE_BYTES.set(size_bytes as i64);
        CLICKHOUSE_DISK_BACKUP_SIZE_BATCHES.set(batches as i64);
    }

    fn increment_backup_disk_errors(_order: &'static str, _error: &str) {
        CLICKHOUSE_BACKUP_DISK_ERRORS.inc();
    }

    fn set_memory_backup_size(size_bytes: u64, batches: usize, _order: &'static str) {
        CLICKHOUSE_MEMORY_BACKUP_SIZE_BYTES.set(size_bytes as i64);
        CLICKHOUSE_MEMORY_BACKUP_SIZE_BATCHES.set(batches as i64);
    }

    fn process_backup_data_lost_quantities(quantities: &Quantities) {
        CLICKHOUSE_ROWS_LOST.inc_by(quantities.rows);
        CLICKHOUSE_BYTES_LOST.inc_by(quantities.bytes);
    }

    fn process_backup_data_quantities(quantities: &Quantities) {
        CLICKHOUSE_ROWS_COMMITTED_FROM_BACKUP.inc_by(quantities.rows);
        CLICKHOUSE_BYTES_COMMITTED_FROM_BACKUP.inc_by(quantities.bytes);
    }
}
