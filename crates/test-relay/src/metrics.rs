use ctor::ctor;
use lazy_static::lazy_static;
use metrics_macros::register_metrics;
use prometheus::{HistogramOpts, HistogramVec, IntCounterVec, Opts, Registry};
use rbuilder::{
    telemetry::{exponential_buckets_range, gather_prometheus_metrics, linear_buckets_range},
    utils::duration_ms,
};
use std::{net::SocketAddr, time::Duration};
use warp::{reject::Rejection, reply::Reply, Filter};

lazy_static! {
    pub static ref REGISTRY: Registry = Registry::new();
}

register_metrics! {
    // Statistics about finalized blocks
    pub static PAYLOADS_RECEIVED: IntCounterVec = IntCounterVec::new(
        Opts::new(
            "payloads_received",
            "payloads received"
        ),
        &["builder"]
    )
    .unwrap();

    pub static PAYLOAD_VALIDATION_ERRORS: IntCounterVec = IntCounterVec::new(
        Opts::new(
            "payloads_validation_errors",
            "payloads validation errors"
        ),
        &["builder"]
    )
    .unwrap();

    pub static RELAY_ERRORS: IntCounterVec = IntCounterVec::new(
        Opts::new(
            "relay_errors",
            "errors when fetching data from relays"
        ),
        &["relay"]
    )
    .unwrap();

    pub static WINS: IntCounterVec = IntCounterVec::new(
        Opts::new(
            "wins",
            "wins per builder (relay samples top bid close to the end to the slot)"
        ),
        &["builder"]
    )
    .unwrap();

    pub static WINNER_ADVANTAGE: HistogramVec = HistogramVec::new(
        HistogramOpts::new("winner_advantage", "Percentage of value that winner has over the next best bid by other builders")
            .buckets(linear_buckets_range(0.0, 100.0, 100)),
        &["builder"],
    ).unwrap();


    pub static PAYLOAD_PROCESSING_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("payload_processing_time", "Time to fully process received payload (ms)")
            .buckets(exponential_buckets_range(1.0, 3000.0, 100)),
        &[],
    ).unwrap();

    pub static PAYLOAD_VALIDATION_TIME: HistogramVec = HistogramVec::new(
        HistogramOpts::new("payload_validation_time", "Time to validate received payload (ms)")
            .buckets(exponential_buckets_range(1.0, 3000.0, 100)),
        &[],
    ).unwrap();
}

pub fn inc_payloads_received(builder: &str) {
    PAYLOADS_RECEIVED.with_label_values(&[builder]).inc();
}

pub fn inc_payload_validation_errors(builder: &str) {
    PAYLOAD_VALIDATION_ERRORS
        .with_label_values(&[builder])
        .inc();
}

pub fn inc_relay_errors() {
    RELAY_ERRORS.with_label_values(&[]).inc();
}

pub fn add_winning_bid(builder: &str, advantage: f64) {
    WINS.with_label_values(&[builder]).inc();
    if advantage != 0.0 {
        // we filter 0.0 advantage to filter edge cases like the first bid in the slot, etc.
        WINNER_ADVANTAGE
            .with_label_values(&[builder])
            .observe(advantage);
    }
}

pub fn add_payload_processing_time(duration: Duration) {
    PAYLOAD_PROCESSING_TIME
        .with_label_values(&[])
        .observe(duration_ms(duration));
}

pub fn add_payload_validation_time(duration: Duration) {
    PAYLOAD_VALIDATION_TIME
        .with_label_values(&[])
        .observe(duration_ms(duration));
}

pub fn spawn_metrics_server(address: SocketAddr) {
    let metrics_route = warp::path!("debug" / "metrics" / "prometheus").and_then(metrics_handler);
    tokio::spawn(warp::serve(metrics_route).run(address));
}

async fn metrics_handler() -> Result<impl Reply, Rejection> {
    Ok(gather_prometheus_metrics(&REGISTRY))
}
