//! Metrics is responsible for collecting metrics that are exposed to prometheus
//!
//! Interface to this crate should be a set of simple functions like:
//! fn set_current_block(block: u64);
//!
//! When metric server is spawned is serves prometheus metrics at: /debug/metrics/prometheus

#![allow(unexpected_cfgs)]
use crate::building::BuiltBlockTrace;
use crate::{
    live_builder::block_list_provider::{blocklist_hash, BlockList},
    primitives::mev_boost::MevBoostRelayID,
    utils::build_info::Version,
};
use alloy_primitives::{utils::Unit, U256};
use bigdecimal::num_traits::Pow;
use ctor::ctor;
use lazy_static::lazy_static;
use metrics_macros::register_metrics;
use prometheus::{
    Counter, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;
use time::OffsetDateTime;
use tracing::error;

mod tracing_metrics;

pub use tracing_metrics::*;

const SUBSIDY_ATTEMPT: &str = "attempt";
const SUBSIDY_LANDED: &str = "landed";

const RELAY_ERROR_CONNECTION: &str = "conn";
const RELAY_ERROR_TOO_MANY_REQUESTS: &str = "too_many";
const RELAY_ERROR_OTHER: &str = "other";

const SIM_STATUS_OK: &str = "sim_success";
const SIM_STATUS_FAIL: &str = "sim_fail";

/// We record timestamps only for blocks built within interval of the block timestamp
const BLOCK_METRICS_TIMESTAMP_LOWER_DELTA: time::Duration = time::Duration::seconds(3);
/// We record timestamps only for blocks built within interval of the block timestamp
const BLOCK_METRICS_TIMESTAMP_UPPER_DELTA: time::Duration = time::Duration::seconds(2);

fn is_now_close_to_slot_end(block_timestamp: OffsetDateTime) -> bool {
    let now = OffsetDateTime::now_utc();
    let too_early = now < block_timestamp - BLOCK_METRICS_TIMESTAMP_LOWER_DELTA;
    let too_late = block_timestamp + BLOCK_METRICS_TIMESTAMP_UPPER_DELTA < now;
    !too_early && !too_late
}

lazy_static! {
    pub static ref REGISTRY: Registry = Registry::new();
}

register_metrics! {

    // Statistics about finalized blocks

    pub static BLOCK_BUILT_TXS: HistogramVec = HistogramVec::new(
        HistogramOpts::new("block_built_txs", "Transactions in the built block")
            .buckets(linear_buckets_range(1.0, 1000.0, 100)),
        &["builder_name"]
    )
    .unwrap();
    pub static BLOCK_BUILT_BLOBS: HistogramVec = HistogramVec::new(
        HistogramOpts::new("block_built_blobs", "Blobs in the built block")
            .buckets(linear_buckets_range(1.0, 32.0, 100)),
        &["builder_name"]
    )
    .unwrap();
    pub static BLOCK_BUILT_GAS_USED: HistogramVec = HistogramVec::new(
        HistogramOpts::new("block_built_gas_used", "Gas used in the built block")
            .buckets(exponential_buckets_range(21_000.0, 30_000_000.0, 100)),
        &["builder_name"]
    )
    .unwrap();
    pub static BLOCK_BUILT_SIM_GAS_USED: HistogramVec = HistogramVec::new(
        HistogramOpts::new(
            "block_built_sim_gas_used",
            "Gas used in the built block including failing bundles"
        )
        .buckets(exponential_buckets_range(21_000.0, 30_000_000.0, 100)),
        &["builder_name"]
    )
    .unwrap();


    pub static BLOCK_VALIDATION_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("block_validation_time", "Block Validation Times (ms)")
            .buckets(exponential_buckets_range(1.0, 3000.0, 100)),
        &[]
    )
    .unwrap();



    pub static CURRENT_BLOCK: IntGauge =
        IntGauge::new("current_block", "Current Block").unwrap();
    pub static ORDERPOOL_TXS: IntGauge =
        IntGauge::new("orderpool_txs", "Transactions In The Orderpool").unwrap();
    pub static ORDERPOOL_BUNDLES: IntGauge =
        IntGauge::new("orderpool_bundles", "Bundles In The Orderpool").unwrap();

    pub static ORDERPOOL_ORDERS_RECEIVED: IntCounterVec = IntCounterVec::new(
        Opts::new("orderpool_commands_received", "counter of orders received"),
        &["kind"]
    )
    .unwrap();

    pub static RELAY_ERRORS: IntCounterVec = IntCounterVec::new(
        Opts::new("relay_errors", "counter of relay errors"),
        &["relay", "kind"]
    )
    .unwrap();
    pub static BLOCK_SIM_ERRORS: IntCounter = IntCounter::new("block_sim_errors", "counter of block simulation errors")
    .unwrap();
    pub static SIMULATED_OK_ORDERS: IntCounter =
        IntCounter::new("simulated_ok_orders", "Simulated succeeded orders").unwrap();
    pub static SIMULATED_FAILED_ORDERS: IntCounter =
        IntCounter::new("simulated_failed_orders", "Simulated failed orders").unwrap();
    pub static SIMULATION_GAS_USED: IntCounter =
        IntCounter::new("simulation_gas_used", "Simulation gas used").unwrap();
    pub static ACTIVE_SLOTS: IntCounter =
        IntCounter::new("active_slots", "Slots when builder was active").unwrap();
    pub static INITIATED_SUBMISSIONS: IntCounterVec = IntCounterVec::new(
        Opts::new(
            "initiated_submissions",
            "Number of initiated submissions to the relays"
        ),
        &["optimistic"],
    )
    .unwrap();
    pub static RELAY_SUBMIT_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("relay_submit_time", "Time to send bid to the relay (ms)")
            .buckets(linear_buckets_range(0.0, 3000.0, 50)),
        &["relay"],
    )
    .unwrap();
    pub static VERSION: IntGaugeVec = IntGaugeVec::new(
        Opts::new("version", "Version of the builder"),
        &["git", "git_ref", "build_time_utc"]
    )
    .unwrap();
    pub static BLOCKLIST_HASH: IntGaugeVec = IntGaugeVec::new(
        Opts::new("blocklist_hash", "Blocklist hash"),
        &["hash"]
    )
    .unwrap();
    pub static BLOCKLIST_LEN: IntGauge =
        IntGauge::new("blocklist_len", "Blocklist len").unwrap();
    pub static RELAY_ACCEPTED_SUBMISSIONS: IntCounterVec = IntCounterVec::new(
        Opts::new(
            "relay_accepted_submissions",
            "Number of accepted submissions"
        ),
        &["relay", "optimistic"]
    )
    .unwrap();
    pub static SIMULATION_THREAD_WORK_TIME: IntCounterVec = IntCounterVec::new(
        Opts::new(
            "simulation_thread_work_time",
            "Time spent working in simulation thread (mus)"
        ),
        &["worker_id"]
    )
    .unwrap();
    pub static SIMULATION_THREAD_WAIT_TIME: IntCounterVec = IntCounterVec::new(
        Opts::new(
            "simulation_thread_wait_time",
            "Time spent waiting for input in simulation thread (mus)"
        ),
        &["worker_id"]
    )
    .unwrap();
    pub static PROVIDER_REOPEN_COUNTER: IntCounter = IntCounter::new(
        "provider_reopen_counter", "Counter of provider reopens").unwrap();

    pub static PROVIDER_BAD_REOPEN_COUNTER: IntCounter = IntCounter::new(
        "provider_bad_reopen_counter", "Counter of provider reopens").unwrap();

    pub static TXFETCHER_TRANSACTION_COUNTER: IntCounter = IntCounter::new(
        "txfetcher_transaction_counter", "Counter of transactions fetched by txfetcher service").unwrap();

    pub static TXFETCHER_TRANSACTION_QUERY_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("txfetcher_transaction_query_time", "Time to retrieve a transaction from the txpool (ms)")
            .buckets(exponential_buckets_range(1.0, 3000.0, 100)),
        &[],
    ).unwrap();

     /////////////////////////////////
     // SUBSIDY
     /////////////////////////////////

    /// We decide this at the end of the submission to relays
    pub static SUBSIDIZED_BLOCK_COUNT: IntCounterVec = IntCounterVec::new(
        Opts::new(
            "subsidized_block_count",
            "Subsidized block count"
        ),
        &["kind"],
    ).unwrap();

    /// We decide this at the end of the submission to relays
    /// We expect to see values around .001
    /// We only count subsidized blocks.
    pub static SUBSIDY_VALUE: HistogramVec = HistogramVec::new(
            HistogramOpts::new("subsidy_value", "Subsidy value")
                .buckets(exponential_buckets_range(0.0001, 0.05, 1000)),
        &["kind"],
        )
        .unwrap();

    pub static TOTAL_LANDED_SUBSIDIES_SUM: Counter =
        Counter::new("total_landed_subsidies_sum", "Sum of all total landed subsidies").unwrap();


    // Performance metrics related to E2E latency

    // Metrics for important step of the block processing
    pub static BLOCK_FILL_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("block_fill_time", "Block Fill Times (ms)")
            .buckets(exponential_buckets_range(1.0, 3000.0, 100)),
        &["builder_name"]
    )
    .unwrap();
    pub static BLOCK_FINALIZE_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("block_finalize_time", "Block Finalize Times (ms)")
            .buckets(exponential_buckets_range(1.0, 3000.0, 100)),
        &[]
    )
    .unwrap();
    pub static BLOCK_ROOT_HASH_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("block_root_hash_time", "Block Root Hash Time (ms)")
            .buckets(exponential_buckets_range(1.0, 2000.0, 100)),
        &[]
    )
    .unwrap();
    pub static ORDER_SIMULATION_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("order_simulation_time", "Order Simulation Time (ms)")
            .buckets(exponential_buckets_range(0.01, 200.0, 200)),
        &["builder_name", "status"]
    )
    .unwrap();

    // E2E tracing metrics
    // The goal of these two metrics is:
    // 1. Cover as many lines of code as possible without any gaps.
    // 2. Show E2E latency of the order that could be executed immediately and also arrived towards the end of the slot.
    // The path of order goes as follows:
    // Received -> Simulated -> (builders start to build a block with it) -> block sealed -> block submit started
    pub static ORDER_RECEIVED_TO_SIM_END_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("order_received_to_sim_end_time", "Time between when the order was received and top of the block simulation ended for orders that arrive after slot start. (ms)")
            .buckets(exponential_buckets_range(0.01, 200.0, 200)),
        &["status"]
    )
    .unwrap();
    pub static ORDER_SIM_END_TO_FIRST_BUILD_STARTED_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("order_sim_end_to_first_build_started_time", "Time between when the order simulation ended and the builder started to build first block with it. (ms)")
            .buckets(exponential_buckets_range(0.01, 300.0, 300)),
        &["builder_name"]
    )
    .unwrap();
    pub static ORDER_SIM_END_TO_FIRST_BUILD_STARTED_MIN_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("order_sim_end_to_first_build_started_min_time", "Time between when the order simulation ended and the first builder started to build first block with it. (ms)")
            .buckets(exponential_buckets_range(0.01, 300.0, 300)),
        &["builder_name"]
    )
    .unwrap();
    pub static BLOCK_FILL_START_SEAL_END_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("block_build_start_seal_end_time", "Time between when the block build started and the block sealed ended. (ms)")
            .buckets(exponential_buckets_range(0.01, 500.0, 300)),
        &["builder_name"]
    )
    .unwrap();
    pub static BLOCK_SEAL_END_SUBMIT_START_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("block_seal_end_submit_start_time", "Time between when the block sealed ended and the block submission started. (ms)")
            .buckets(exponential_buckets_range(0.01, 500.0, 300)),
        &[]
    )
    .unwrap();
}

// This function should be called periodically to reset histogram metrics.
// If metrics are not reset histogram quantiles become rigid.
// Reset period is 10 minutes.
pub fn reset_histogram_metrics() {
    const HISTOGRAM_METRIC_RESET_PERIOD: Duration = Duration::from_secs(10 * 60);

    lazy_static! {
        static ref LAST_RESET: Arc<Mutex<Instant>> = Arc::new(Mutex::new(Instant::now()));
    }

    let now = Instant::now();
    let mut last_reset = LAST_RESET.lock().unwrap();
    if now.duration_since(*last_reset) < HISTOGRAM_METRIC_RESET_PERIOD {
        return;
    }
    *last_reset = now;

    // Reset all histogram metrics
    BLOCK_BUILT_TXS.reset();
    BLOCK_BUILT_BLOBS.reset();
    BLOCK_BUILT_GAS_USED.reset();
    BLOCK_BUILT_SIM_GAS_USED.reset();
    BLOCK_VALIDATION_TIME.reset();
    BLOCK_FILL_TIME.reset();
    BLOCK_FINALIZE_TIME.reset();
    BLOCK_ROOT_HASH_TIME.reset();
    ORDER_SIMULATION_TIME.reset();
    RELAY_SUBMIT_TIME.reset();
    TXFETCHER_TRANSACTION_QUERY_TIME.reset();
    SUBSIDY_VALUE.reset();
    ORDER_RECEIVED_TO_SIM_END_TIME.reset();
    ORDER_SIM_END_TO_FIRST_BUILD_STARTED_TIME.reset();
    ORDER_SIM_END_TO_FIRST_BUILD_STARTED_MIN_TIME.reset();
    BLOCK_FILL_START_SEAL_END_TIME.reset();
    BLOCK_SEAL_END_SUBMIT_START_TIME.reset();
}

pub(super) fn set_version(version: Version) {
    VERSION
        .with_label_values(&[
            &version.git_commit,
            &version.git_ref,
            &version.build_time_utc,
        ])
        .set(1);
}

pub fn update_blocklist_metrics(blocklist: &BlockList) {
    let hash = blocklist_hash(blocklist).to_string();
    BLOCKLIST_HASH
        .with_label_values(&[&hash[2..] /* remove the 0x */])
        .set(1);
    BLOCKLIST_LEN.set(blocklist.len() as i64);
}

pub fn inc_other_relay_errors(relay: &MevBoostRelayID) {
    RELAY_ERRORS
        .with_label_values(&[relay.as_str(), RELAY_ERROR_OTHER])
        .inc()
}

pub fn inc_conn_relay_errors(relay: &MevBoostRelayID) {
    RELAY_ERRORS
        .with_label_values(&[relay.as_str(), RELAY_ERROR_CONNECTION])
        .inc()
}

pub fn inc_too_many_req_relay_errors(relay: &MevBoostRelayID) {
    RELAY_ERRORS
        .with_label_values(&[relay.as_str(), RELAY_ERROR_TOO_MANY_REQUESTS])
        .inc()
}

pub fn inc_failed_block_simulations() {
    BLOCK_SIM_ERRORS.inc()
}

pub fn set_current_block(block: u64) {
    CURRENT_BLOCK.set(block as i64);
}

/// Simulated orders on top of block as first step of block building.
pub fn inc_simulated_orders(ok: bool) {
    if ok {
        SIMULATED_OK_ORDERS.inc();
    } else {
        SIMULATED_FAILED_ORDERS.inc();
    }
}

/// Gas used in any context of block building
pub fn inc_simulation_gas_used(gas: u64) {
    SIMULATION_GAS_USED.inc_by(gas);
}

pub fn set_ordepool_count(txs: usize, bundles: usize) {
    ORDERPOOL_TXS.set(txs as i64);
    ORDERPOOL_BUNDLES.set(bundles as i64);
}

#[allow(clippy::too_many_arguments)]
pub fn add_finalized_block_metrics(
    built_block_trace: &BuiltBlockTrace,
    txs: usize,
    blobs: usize,
    gas_used: u64,
    sim_gas_used: u64,
    builder_name: &str,
    block_timestamp: OffsetDateTime,
) {
    if !is_now_close_to_slot_end(block_timestamp) {
        return;
    }

    BLOCK_FINALIZE_TIME
        .with_label_values(&[])
        .observe(built_block_trace.finalize_time.as_micros() as f64 / 1000.0);
    BLOCK_ROOT_HASH_TIME
        .with_label_values(&[])
        .observe(built_block_trace.root_hash_time.as_micros() as f64 / 1000.0);

    BLOCK_BUILT_TXS
        .with_label_values(&[builder_name])
        .observe(txs as f64);
    BLOCK_BUILT_BLOBS
        .with_label_values(&[builder_name])
        .observe(blobs as f64);
    BLOCK_BUILT_GAS_USED
        .with_label_values(&[builder_name])
        .observe(gas_used as f64);
    BLOCK_BUILT_SIM_GAS_USED
        .with_label_values(&[builder_name])
        .observe(sim_gas_used as f64);

    let build_start_seal_end_time =
        (built_block_trace.orders_sealed_at - built_block_trace.orders_closed_at).as_seconds_f64()
            * 1000.0;
    BLOCK_FILL_START_SEAL_END_TIME
        .with_label_values(&[builder_name])
        .observe(build_start_seal_end_time);
}

pub fn add_block_fill_time(
    duration: Duration,
    builder_name: &str,
    block_timestamp: OffsetDateTime,
) {
    if !is_now_close_to_slot_end(block_timestamp) {
        return;
    }
    BLOCK_FILL_TIME
        .with_label_values(&[builder_name])
        .observe(duration.as_micros() as f64 / 1000.0);
}

pub fn add_block_validation_time(duration: Duration) {
    BLOCK_VALIDATION_TIME
        .with_label_values(&[])
        .observe(duration.as_millis() as f64);
}

pub fn inc_active_slots() {
    ACTIVE_SLOTS.inc();
}

pub fn inc_initiated_submissions(optimistic: bool) {
    INITIATED_SUBMISSIONS
        .with_label_values(&[&optimistic.to_string()])
        .inc()
}

pub fn add_relay_submit_time(relay: &MevBoostRelayID, duration: Duration) {
    RELAY_SUBMIT_TIME
        .with_label_values(&[relay.as_str()])
        .observe(duration.as_millis() as f64);
}

pub fn inc_relay_accepted_submissions(relay: &MevBoostRelayID, optimistic: bool) {
    RELAY_ACCEPTED_SUBMISSIONS
        .with_label_values(&[relay.as_str(), &optimistic.to_string()])
        .inc();
}

pub fn add_txfetcher_time_to_query(duration: Duration) {
    TXFETCHER_TRANSACTION_QUERY_TIME
        .with_label_values(&[])
        .observe(duration.as_millis() as f64);

    TXFETCHER_TRANSACTION_COUNTER.inc();
}

pub fn inc_provider_reopen_counter() {
    PROVIDER_REOPEN_COUNTER.inc();
}

pub fn inc_provider_bad_reopen_counter() {
    PROVIDER_BAD_REOPEN_COUNTER.inc();
}

pub fn add_sim_thread_utilisation_timings(
    work_time: Duration,
    wait_time: Duration,
    thread_id: usize,
) {
    SIMULATION_THREAD_WORK_TIME
        .with_label_values(&[&thread_id.to_string()])
        .inc_by(work_time.as_micros() as u64);
    SIMULATION_THREAD_WAIT_TIME
        .with_label_values(&[&thread_id.to_string()])
        .inc_by(wait_time.as_micros() as u64);
}

/// landed vs attempt
fn subsidized_label(landed: bool) -> &'static str {
    if landed {
        SUBSIDY_LANDED
    } else {
        SUBSIDY_ATTEMPT
    }
}

pub fn inc_subsidized_blocks(landed: bool) {
    SUBSIDIZED_BLOCK_COUNT
        .with_label_values(&[subsidized_label(landed)])
        .inc();
}

pub fn add_subsidy_value(value: U256, landed: bool) {
    let value_float = 2.0_f64.powf(value.approx_log2()) / 10_f64.pow(Unit::ETHER.get());
    SUBSIDY_VALUE
        .with_label_values(&[subsidized_label(landed)])
        .observe(value_float);
    if landed {
        TOTAL_LANDED_SUBSIDIES_SUM.inc_by(value_float);
    }
}

fn sim_status(success: bool) -> &'static str {
    if success {
        SIM_STATUS_OK
    } else {
        SIM_STATUS_FAIL
    }
}

pub fn add_order_simulation_time(duration: Duration, builder_name: &str, success: bool) {
    ORDER_SIMULATION_TIME
        .with_label_values(&[builder_name, sim_status(success)])
        .observe(duration.as_micros() as f64 / 1000.0);
}

pub fn mark_submission_start_time(block_sealed_at: OffsetDateTime) {
    // we don't check if we are close to slot end because submission code handles that
    let now = OffsetDateTime::now_utc();
    let value = (now - block_sealed_at).as_seconds_f64() * 1000.0;
    BLOCK_SEAL_END_SUBMIT_START_TIME
        .with_label_values(&[])
        .observe(value);
}

pub fn gather_prometheus_metrics(registry: &Registry) -> String {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();

    let mut buffer = Vec::new();
    if let Err(e) = encoder.encode(&registry.gather(), &mut buffer) {
        error!("could not encode custom metrics: {}", e);
    };
    let mut res = String::from_utf8(buffer.clone()).unwrap_or_else(|e| {
        error!("custom metrics could not be from_utf8'd: {}", e);
        String::default()
    });
    buffer.clear();

    let mut buffer = Vec::new();
    if let Err(e) = encoder.encode(&prometheus::gather(), &mut buffer) {
        error!("could not encode prometheus metrics: {}", e);
    };
    let res_custom = String::from_utf8(buffer.clone()).unwrap_or_else(|e| {
        error!("prometheus metrics could not be from_utf8'd: {}", e);
        String::default()
    });
    buffer.clear();
    res.push_str(&res_custom);
    res
}

// Creates n exponential buckets that cover range from start to end.
pub fn exponential_buckets_range(start: f64, end: f64, n: usize) -> Vec<f64> {
    assert!(start > 0.0 && start < end);
    assert!(n > 1);
    let factor = (end / start).powf(1.0 / (n - 1) as f64);
    prometheus::exponential_buckets(start, factor, n).unwrap()
}

// Creates n linear buckets that cover range from [start, end].
pub fn linear_buckets_range(start: f64, end: f64, n: usize) -> Vec<f64> {
    assert!(start < end);
    let width = (end - start) / (n - 1) as f64;
    prometheus::linear_buckets(start, width, n).unwrap()
}
