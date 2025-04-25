//! Telemetry helps track what is happening in the running application using metrics and tracing.
//!
//! Interface to telemetry should be set of simple functions like:
//! fn record_event(event_data)
//!
//! All internals are global variables.
//!
//! Full server may expose metrics that could leak information when running tdx.

use std::net::SocketAddr;
use warp::{Filter, Rejection, Reply};

use crate::{
    telemetry::{
        metrics::{gather_prometheus_metrics, set_version},
        REGISTRY,
    },
    utils::build_info::Version,
};

pub async fn spawn(addr: SocketAddr, version: Version) -> eyre::Result<()> {
    set_version(version);

    // metrics over /debug/metrics/prometheus
    let metrics_route = warp::path!("debug" / "metrics" / "prometheus").and_then(metrics_handler);
    tokio::spawn(warp::serve(metrics_route).run(addr));

    Ok(())
}

async fn metrics_handler() -> Result<impl Reply, Rejection> {
    Ok(gather_prometheus_metrics(&REGISTRY))
}
