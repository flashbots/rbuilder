//! Server that only exposes redacted data, suitable for being exposed by tdx
//! builders in real-time.
//!
//! Currently exposes just a healthcheck endpoint on /health. Can be extended
//! in the future.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use warp::{Filter, Rejection, Reply};

///  ready_to_build should be externally set to true so readyz answers OK and not SERVICE_UNAVAILABLE
pub async fn spawn(addr: SocketAddr, ready_to_build: Arc<AtomicBool>) -> eyre::Result<()> {
    let ready_to_build_clone = ready_to_build.clone();

    let livez = warp::path!("livez").and(warp::get()).and_then(handle_livez);

    let readyz = warp::path!("readyz").and(warp::get()).and_then(move || {
        let ready_to_build = ready_to_build_clone.clone();
        async move { handle_readyz(ready_to_build).await }
    });

    let routes = livez.or(readyz);

    tokio::spawn(warp::serve(routes).run(addr));
    Ok(())
}

async fn handle_livez() -> Result<impl Reply, Rejection> {
    Ok(warp::http::StatusCode::OK)
}

async fn handle_readyz(ready_to_build: Arc<AtomicBool>) -> Result<impl Reply, Rejection> {
    Ok(if ready_to_build.load(Ordering::Relaxed) {
        warp::http::StatusCode::OK
    } else {
        warp::http::StatusCode::SERVICE_UNAVAILABLE
    })
}
