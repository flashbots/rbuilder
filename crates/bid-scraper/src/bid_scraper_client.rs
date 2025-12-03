use futures_retry::{FutureRetry, RetryPolicy};
use runng::{
    asyncio::{AsyncSocket, ReadAsync},
    latest::ProtocolFactory,
    protocol::Subscribe,
    Dial,
};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::types::ScrapedRelayBlockBid;

/// Sink for scraped bids.
pub trait ScrapedBidsObs: Send + Sync {
    /// Be careful, we don't assume any kind of filtering here so bid may contain our own bids.
    fn update_new_bid(&self, bid: ScrapedRelayBlockBid);
}

/// NNG subscriber with infinite retries.
/// timeout: if we don't get a new bid in this time we reconnect.
/// retry_wait: time we wait to reconnect.
pub async fn run_nng_subscriber_with_retries(
    obs: Arc<dyn ScrapedBidsObs>,
    cancel: CancellationToken,
    publisher_url: String,
    timeout: Duration,
    retry_wait: Duration,
) {
    let url = publisher_url.clone(); // for reuse in error handler
    tokio::select! {
        result = FutureRetry::new(
            move || run_nng_subscriber(obs.clone(), publisher_url.clone(), timeout),
            move |error: Box<dyn std::error::Error>| {
                tracing::error!(url,?error, "Subscriber returned an error");
                RetryPolicy::<()>::WaitRetry(retry_wait)
            },
        ) => {
            let attempts = match result {
                Ok((_, attempts)) => attempts,
                Err((_, attempts)) => attempts,
            };
            unreachable!("NNG subscription exited after {attempts} attempts")
        }
        _ = cancel.cancelled() => {
            info!("bid scraper NNG subscription cancelled");
        }
    }
}

/// NNG subscriber that forwards bids to the channel.
async fn run_nng_subscriber(
    obs: Arc<dyn ScrapedBidsObs>,
    publisher_url: String,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut socket = ProtocolFactory::default().subscriber_open()?;
    socket.dial(&publisher_url)?;
    socket.subscribe_str("").expect("failed to subscribe");

    let mut nng_reader = socket.create_async()?;
    tracing::info!(publisher_url, "Created nanomsg socket and subscribed");

    loop {
        let msg = tokio::time::timeout(timeout, nng_reader.receive()).await??;
        let block_bid: ScrapedRelayBlockBid = serde_json::from_slice(msg.body())?;
        obs.update_new_bid(block_bid);
    }
}
