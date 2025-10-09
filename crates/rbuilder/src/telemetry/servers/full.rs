//! Telemetry helps track what is happening in the running application using metrics and tracing.
//!
//! Interface to telemetry should be set of simple functions like:
//! fn record_event(event_data)
//!
//! All internals are global variables.
//!
//! Full server may expose metrics that could leak information when running tdx.

use std::net::SocketAddr;
use time::OffsetDateTime;
use warp::{Filter, Rejection, Reply};

use crate::{
    telemetry::{
        metrics::{gather_prometheus_metrics, set_version},
        BUILDER_BALANCE, CURRENT_BLOCK, MAX_FRESH_GAUGE_AGE, ORDERPOOL_BUNDLES, ORDERPOOL_TXS,
        ORDERPOOL_TXS_SIZE, REGISTRY,
    },
    utils::build_info::Version,
};

pub async fn spawn(addr: SocketAddr, version: Version) -> eyre::Result<()> {
    set_version(version);
    tokio::spawn(async move {
        loop {
            let now = OffsetDateTime::now_utc();
            BUILDER_BALANCE.check_if_fresh(now);
            CURRENT_BLOCK.check_if_fresh(now);
            ORDERPOOL_TXS.check_if_fresh(now);
            ORDERPOOL_BUNDLES.check_if_fresh(now);
            ORDERPOOL_TXS_SIZE.check_if_fresh(now);
            tokio::time::sleep(MAX_FRESH_GAUGE_AGE / 2).await;
        }
    });

    // metrics over /debug/metrics/prometheus
    let metrics_route = warp::path!("debug" / "metrics" / "prometheus").and_then(metrics_handler);
    tokio::spawn(warp::serve(metrics_route).run(addr));

    Ok(())
}

async fn metrics_handler() -> Result<impl Reply, Rejection> {
    Ok(gather_prometheus_metrics(&REGISTRY))
}
