//! Server that only exposes redacted data, suitable for being exposed by tdx
//! builders in real-time.
//!
//! Currently exposes just a healthcheck endpoint on /health. Can be extended
//! in the future.

use std::net::SocketAddr;

use warp::{Filter, Rejection, Reply};

async fn handler() -> Result<impl Reply, Rejection> {
    Ok("OK")
}

pub async fn spawn(addr: SocketAddr) -> eyre::Result<()> {
    let route = warp::path!("health").and_then(handler);
    tokio::spawn(warp::serve(route).run(addr));
    Ok(())
}
