//! Telemetry helps track what is happening in the running application using metrics and tracing.
//!
//! Interface to telemetry should be set of simple functions like:
//! fn record_event(event_data)
//! All internals are global variables.

use serde::Deserialize;
use std::{net::SocketAddr, path::PathBuf};
use tracing::{info, warn};
use warp::{Filter, Rejection, Reply};

mod dynamic_logs;
pub mod metrics;

pub use dynamic_logs::*;
pub use metrics::*;

use crate::utils::build_info::Version;

async fn metrics_handler() -> Result<impl Reply, Rejection> {
    Ok(gather_prometheus_metrics())
}

#[derive(Debug, Deserialize)]
struct LogQuery {
    file: Option<PathBuf>,
}

async fn set_rust_log_handle(
    rust_log: String,
    log_query: LogQuery,
) -> Result<impl Reply, Rejection> {
    info!(?rust_log, ?log_query, "Setting log level");
    let mut log_config = default_log_config();
    log_config.file.clone_from(&log_query.file);
    log_config.env_filter.clone_from(&rust_log);
    match set_log_config(log_config) {
        Ok(_) => Ok("".to_string()),
        Err(err) => {
            warn!(?err, ?rust_log, ?log_query, "Failed to set log level");
            Ok(err.to_string())
        }
    }
}

async fn reset_log_handle() -> Result<impl Reply, Rejection> {
    info!("Resetting log level");
    match reset_log_config() {
        Ok(_) => Ok("".to_string()),
        Err(err) => {
            warn!(?err, "Failed to reset log level");
            Ok(err.to_string())
        }
    }
}

pub async fn spawn_telemetry_server(addr: SocketAddr, version: Version) -> eyre::Result<()> {
    register_custom_metrics();

    set_version(version);

    // metrics over /debug/metrics/prometheus
    let metrics_route = warp::path!("debug" / "metrics" / "prometheus").and_then(metrics_handler);

    let log_set_route = warp::path!("debug" / "log" / "set" / String)
        .and(warp::query::<LogQuery>())
        .and_then(set_rust_log_handle);
    let log_reset_route = warp::path!("debug" / "log" / "reset").and_then(reset_log_handle);

    let route = metrics_route.or(log_set_route).or(log_reset_route);

    tokio::spawn(warp::serve(route).run(addr));

    Ok(())
}
