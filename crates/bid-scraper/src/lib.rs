use std::time::Duration;

pub mod bids_publisher;
pub mod bloxroute_ws_publisher;
mod slot;
pub mod ultrasound_ws_publisher;

pub mod best_bid_ws_connector;
pub mod bid_scraper;
pub mod bid_scraper_client;
pub mod bid_sender;
pub mod config;
pub mod headers_publisher;
pub mod reconnect;
pub mod relay_api_publisher;
pub mod types;
pub mod ws_publisher;

pub type DynResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub const RPC_TIMEOUT: Duration = Duration::from_secs(60);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

pub fn get_timestamp_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}
