//! EPBS Builder API HTTP Server.

use alloy_primitives::BlockHash;
use axum::{routing::get, Router};
use parking_lot::RwLock;
use rbuilder_primitives::epbs::{
    CachedPayloadData, GetBidParams, SignedExecutionPayloadBid,
};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::handlers::{get_bid_handler, status_handler};

#[derive(Debug, Clone)]
pub struct EpbsBuilderServerConfig {
    /// server address
    pub listen_addr: SocketAddr,
    /// max age for cached payloads before they are evicted.
    pub cache_ttl: Duration,
}

impl Default for EpbsBuilderServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:18551".parse().unwrap(),
            cache_ttl: Duration::from_secs(32 * 12), // setting upto 2 epochs
        }
    }
}

/// Trait for generating EPBS bids.
///
/// This trait is implemented by the block builder to provide bids
/// to the EPBS Builder API server.
#[async_trait::async_trait]
pub trait EpbsBidProvider: Send + Sync {
    /// generates the signed execution payload and returns it if no error encountered
    /// returns none if no bid can be generated (e.g., unknown slot, no payload ready).
    async fn generate_bid(
        &self,
        params: &GetBidParams,
    ) -> eyre::Result<Option<SignedExecutionPayloadBid>>;
}

/// State shared between the HTTP server and handlers.
pub struct EpbsBuilderState {
    /// builder server config
    pub config: EpbsBuilderServerConfig,
    /// bid provider (block builder integration).
    bid_provider: Arc<dyn EpbsBidProvider>,
    /// cache the generated payloads, keyed by block_hash.
    /// when a bid is returned, the full payload is cached here
    /// so it can be revealed when the beacon block is seen.
    payload_cache: RwLock<HashMap<BlockHash, CachedPayloadData>>,
}

impl std::fmt::Debug for EpbsBuilderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpbsBuilderState")
            .field("config", &self.config)
            .field("payload_cache_len", &self.payload_cache.read().len())
            .finish()
    }
}

impl EpbsBuilderState {
    pub fn new(config: EpbsBuilderServerConfig, bid_provider: Arc<dyn EpbsBidProvider>) -> Self {
        Self {
            config,
            bid_provider,
            payload_cache: RwLock::new(HashMap::new()),
        }
    }

    /// returns a bid given the bid params
    pub async fn get_bid(
        &self,
        params: &GetBidParams,
    ) -> eyre::Result<Option<SignedExecutionPayloadBid>> {
        self.bid_provider.generate_bid(params).await
    }

    /// cache a payload for later revelation.
    pub fn cache_payload(&self, data: CachedPayloadData) {
        let block_hash = data.bid.message.block_hash;
        self.payload_cache.write().insert(block_hash, data);
    }

    pub fn get_cached_payload(&self, block_hash: &BlockHash) -> Option<CachedPayloadData> {
        self.payload_cache.read().get(block_hash).cloned()
    }

    pub fn cleanup_cache(&self) {
        let ttl = self.config.cache_ttl;
        self.payload_cache
            .write()
            .retain(|_, v| v.created_at.elapsed() < ttl);
    }
}

/// EPBS Builder API HTTP Server.
#[derive(Debug)]
pub struct EpbsBuilderServer {
    state: Arc<EpbsBuilderState>,
}

impl EpbsBuilderServer {
    pub fn new(config: EpbsBuilderServerConfig, bid_provider: Arc<dyn EpbsBidProvider>) -> Self {
        Self {
            state: Arc::new(EpbsBuilderState::new(config, bid_provider)),
        }
    }

    pub fn state(&self) -> Arc<EpbsBuilderState> {
        self.state.clone()
    }

    /// Returns the listen address for this server.
    pub fn listen_addr(&self) -> std::net::SocketAddr {
        self.state.config.listen_addr
    }

    fn build_router(&self) -> Router {
        Router::new()
            .route(
                "/eth/v1/builder/execution_payload_bid/:slot/:parent_hash/:parent_root/:proposer_index",
                get(get_bid_handler),
            )
            .route("/eth/v1/builder/status", get(status_handler))
            .with_state(self.state.clone())
    }

    pub async fn run(self, cancel: CancellationToken) -> eyre::Result<()> {
        let addr = self.state.config.listen_addr;
        let router = self.build_router();

        info!("starting builder server for epbs bids {}", addr);

        let listener = TcpListener::bind(addr).await?;

        // spawn cache cleanup task
        let state_clone = self.state.clone();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                tokio::select! {
                    _ = cancel_clone.cancelled() => break,
                    _ = interval.tick() => {
                        state_clone.cleanup_cache();
                    }
                }
            }
        });

        // run the server
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                cancel.cancelled().await;
                info!("shutting down builder server");
            })
            .await?;

        Ok(())
    }
}


