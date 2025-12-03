use crate::{
    telemetry::{exponential_buckets_range, REGISTRY},
    utils,
};
use alloy_primitives::{bytes::Bytes, B256};
use alloy_rpc_types_beacon::relay::SubmitBlockRequest as AlloySubmitBlockRequest;
use alloy_rpc_types_beacon::BlsPublicKey;
use ctor::ctor;
use lazy_static::lazy_static;
use metrics_macros::register_metrics;
use parking_lot::Mutex;
use prometheus::{HistogramOpts, HistogramVec, IntCounter};
use rbuilder_primitives::mev_boost::{verify_signed_relay_request, SignedGetPayloadV3};
use schnellru::{ByLength, LruMap};
use ssz::{Decode as _, Encode};
use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio_util::sync::CancellationToken;
use tracing::*;
use warp::{
    http::{header::CONTENT_TYPE, HeaderValue, StatusCode},
    Filter,
};

register_metrics! {
    pub static REQUESTS_TOTAL: IntCounter = IntCounter::new("relay_server_requests", "The total number of requests on the optimistic V3 relay server").unwrap();
     pub static RELAY_REQUEST_LATENCY: HistogramVec = HistogramVec::new(
        HistogramOpts::new("relay_server_relay_request_latency", "The number of milliseconds elapsed since relay request timestamp")
            .buckets(exponential_buckets_range(0.01, 300.0, 200)),
        &[]
    ).unwrap();
    pub static RESPONSE_LATENCY: HistogramVec = HistogramVec::new(
        HistogramOpts::new("relay_server_response_latency", "The number of milliseconds for returning optimistic V3 relay response")
            .buckets(exponential_buckets_range(0.01, 300.0, 200)),
        &[]
    ).unwrap();
   pub static BAD_REQUESTS_TOTAL: IntCounter = IntCounter::new("relay_server_bad_requests", "The total number of bad requests on the optimistic V3 relay server").unwrap();
   pub static UNKNOWN_PUBKEY_TOTAL: IntCounter = IntCounter::new("relay_server_unknown_pubkey", "The total number of unknown pubkey errors on the optimistic V3 relay server").unwrap();
   pub static INVALID_SIGNATURE_TOTAL: IntCounter = IntCounter::new("relay_server_invalid_signature", "The total number of invalid signature errors on the optimistic V3 relay server").unwrap();
   pub static BLOCK_NOT_FOUND_TOTAL: IntCounter = IntCounter::new("relay_server_block_not_found", "The total number of block not found errors on the optimistic V3 relay server").unwrap();
}

/// The default number of blocks to keep in cache. With bid update latency of 1ms this would preserve the last second worth of submitted blocks.
pub const OPTIMISTIC_V3_CACHE_SIZE_DEFAULT: u32 = 1_000;

/// The content length limit for the incoming relay requests. The actual raw data should fit in roughly
/// 96 (signature) + 32 (block hash) + 8 (timestamp) + 48 (pubkey) bytes plus the encoding overhead.
pub const OPTIMISTIC_V3_SERVER_CONTENT_LENGTH_LIMIT: u64 = 1_024;

/// Endpoint for returning for block payloads to the relays.
/// Reference: <https://ethresear.ch/t/introduction-to-optimistic-v3-relays/22066#p-53641-technical-specification-8>
pub const GET_PAYLOAD_V3: &str = "get_payload_v3";

/// The cache containing blocks submitted via optimistic V3.
#[derive(Clone, Debug)]
pub struct OptimisticV3BlockCache(Arc<Mutex<LruMap<B256, Arc<AlloySubmitBlockRequest>>>>);

impl Default for OptimisticV3BlockCache {
    fn default() -> Self {
        Self::new(OPTIMISTIC_V3_CACHE_SIZE_DEFAULT)
    }
}

impl OptimisticV3BlockCache {
    /// Create new cache with provided size.
    pub fn new(size: u32) -> Self {
        Self(Arc::new(Mutex::new(LruMap::new(ByLength::new(size)))))
    }

    pub fn insert(&self, block: Arc<AlloySubmitBlockRequest>) {
        let block_hash = block.bid_trace().block_hash;
        self.0.lock().insert(block_hash, block);
    }

    pub fn get(&self, hash: &B256) -> Option<Arc<AlloySubmitBlockRequest>> {
        self.0.lock().get(hash).cloned()
    }
}

/// Initialize the HTTP server.
pub fn spawn_server(
    address: impl Into<SocketAddr>,
    domain: B256,
    relay_pubkeys: HashSet<BlsPublicKey>,
    blocks: OptimisticV3BlockCache,
    cancellation: CancellationToken,
) -> eyre::Result<()> {
    // Spawn relay server.
    let handler = Handler {
        domain,
        relay_pubkeys,
        blocks,
    };
    let address = address.into();
    let path = warp::path(GET_PAYLOAD_V3)
        .and(warp::post())
        .and(warp::any().map(move || handler.clone()))
        .and(warp::header::<String>("content-type"))
        .and(warp::body::content_length_limit(
            OPTIMISTIC_V3_SERVER_CONTENT_LENGTH_LIMIT,
        ))
        .and(warp::body::bytes())
        .map(Handler::get_payload_v3_metered);

    let (_, server) = warp::serve(path).bind_with_graceful_shutdown(address, async move {
        cancellation.cancelled().await;
        let now = std::time::Instant::now();
        info!(target: "relay_server", "Received cancellation, initiating graceful shutdown");
        // Sleep for 12 seconds to avoid being demoted for the current slot.
        tokio::time::sleep(Duration::from_secs(12)).await;
        info!(target: "relay_server", elapsed = ?now.elapsed(), "Graceful shutdown complete");
    });
    tokio::spawn(server);
    info!(target: "relay_server", %address, "Relay server listening");

    Ok(())
}

#[derive(Clone, Debug)]
struct Handler {
    domain: B256,
    relay_pubkeys: HashSet<BlsPublicKey>,
    blocks: OptimisticV3BlockCache,
}

impl Handler {
    fn get_payload_v3_metered(
        self,
        content_type: String,
        bytes: Bytes,
    ) -> Result<warp::reply::Response, StatusCode> {
        REQUESTS_TOTAL.inc();
        let start = Instant::now();
        let response = Self::get_payload_v3(self, content_type, bytes);
        RESPONSE_LATENCY
            .with_label_values(&[])
            .observe(utils::duration_ms(start.elapsed()));
        response
    }

    fn get_payload_v3(
        self,
        content_type: String,
        bytes: Bytes,
    ) -> Result<warp::reply::Response, StatusCode> {
        let mut is_json = false;
        let request: SignedGetPayloadV3 = if content_type == "application/json" {
            is_json = true;
            serde_json::from_slice(&bytes).map_err(|error| {
                error!(target: "relay_server", ?error, "error parsing json request");
                BAD_REQUESTS_TOTAL.inc();
                StatusCode::BAD_REQUEST
            })?
        } else if content_type == "application/octet-stream" {
            SignedGetPayloadV3::from_ssz_bytes(&bytes).map_err(|error| {
                error!(target: "relay_server", ?error, "error parsing ssz request");
                BAD_REQUESTS_TOTAL.inc();
                StatusCode::BAD_REQUEST
            })?
        } else {
            error!(target: "relay_server", %content_type, "invalid content type");
            BAD_REQUESTS_TOTAL.inc();
            return Err(StatusCode::BAD_REQUEST);
        };

        if let Ok(relay_latency) = SystemTime::now().duration_since(
            std::time::UNIX_EPOCH + Duration::from_millis(request.message.request_ts),
        ) {
            RELAY_REQUEST_LATENCY
                .with_label_values(&[])
                .observe(relay_latency.as_millis() as f64);
        }

        let relay_pubkey = request.message.relay_public_key;
        let block_hash = request.message.block_hash;
        debug!(target: "relay_server", %relay_pubkey, %block_hash, "Serving get payload request");

        if !self.relay_pubkeys.contains(&relay_pubkey) {
            UNKNOWN_PUBKEY_TOTAL.inc();
            debug!(target: "relay_server", %relay_pubkey, "unknown relay pubkey");
            return Err(StatusCode::UNAUTHORIZED);
        }

        if let Err(error) = verify_signed_relay_request(&request, self.domain) {
            INVALID_SIGNATURE_TOTAL.inc();
            debug!(target: "relay_server", %relay_pubkey, ?error, "error verifying request signature");
            return Err(StatusCode::UNAUTHORIZED);
        }

        let block = self.blocks.get(&block_hash).ok_or_else(|| {
            debug!(target: "relay_server", %relay_pubkey, %block_hash, "block not found");
            BLOCK_NOT_FOUND_TOTAL.inc();
            StatusCode::NOT_FOUND
        })?;

        let (body, content_ty) = if is_json {
            let json = serde_json::to_vec(&block).map_err(|error| {
                error!(target: "relay_server", %relay_pubkey, %block_hash, ?error, "error serializing the block");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            (json, "application/json")
        } else {
            let ssz = block.as_ssz_bytes();
            (ssz, "application/octet-stream")
        };

        debug!(target: "relay_server", %relay_pubkey, %block_hash, "Returning payload for request");
        let mut res = warp::http::Response::new(body.into());
        res.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static(content_ty));
        Ok(res)
    }
}
