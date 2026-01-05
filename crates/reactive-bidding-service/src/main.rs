//! Reactive Bidding Service
//!
//! A service that monitors relay bid streams and can:
//! 1. Record auction data for analysis (debug mode)
//! 2. React to competitor bids with counter-bids (bidding mode)

mod config;
mod recorder;
mod strategy;

use std::{path::PathBuf, sync::Arc};

use bid_scraper::{
    bid_sender::{BidSender, BidSenderError},
    config::{NamedPublisherConfig, PublisherConfig},
    types::ScrapedRelayBlockBid,
    ultrasound_ws_publisher::UltrasoundWsPublisherConfig,
};
use eyre::Result;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::config::{load_config, RelayConfig};
use crate::recorder::{create_recorder, BlockRecorder};
use crate::strategy::{create_strategy, BiddingStrategy, CounterBidReceiver};

/// BidSender implementation that sends bids through a broadcast channel
pub struct BroadcastBidSender {
    tx: broadcast::Sender<ScrapedRelayBlockBid>,
}

impl BroadcastBidSender {
    pub fn new(tx: broadcast::Sender<ScrapedRelayBlockBid>) -> Self {
        Self { tx }
    }
}

impl BidSender for BroadcastBidSender {
    fn send(&self, bid: ScrapedRelayBlockBid) -> Result<(), BidSenderError> {
        let _ = self.tx.send(bid);
        Ok(())
    }
}

/// Convert config to bid-scraper's NamedPublisherConfig format
fn convert_relay_config(relays: &[RelayConfig]) -> Vec<NamedPublisherConfig> {
    relays
        .iter()
        .map(|relay| NamedPublisherConfig {
            name: relay.name.clone(),
            publisher: PublisherConfig::UltrasoundWs(UltrasoundWsPublisherConfig {
                ultrasound_url: relay.url.clone(),
                relay_name: relay.name.clone(),
                builder_id: relay.builder_id.clone(),
                api_token: relay.api_token.clone(),
            }),
        })
        .collect()
}

/// Process incoming bids - records and evaluates strategy
async fn process_bids(
    mut bid_rx: broadcast::Receiver<ScrapedRelayBlockBid>,
    recorder: Option<Arc<BlockRecorder>>,
    strategy: Arc<BiddingStrategy>,
) {
    info!("Starting bid processor...");

    while let Ok(bid) = bid_rx.recv().await {
        // Record bid if in debug mode
        if let Some(ref rec) = recorder {
            rec.record_bid(&bid);
        }

        // Evaluate bidding strategy (sends to channel if counter-bid needed)
        strategy.on_bid(&bid);
    }
}

/// Handle counter-bids from the strategy
/// This is where you would integrate with the builder to submit bids
async fn handle_counter_bids(mut rx: CounterBidReceiver) {
    info!("Starting counter-bid handler...");

    while let Some(counter_bid) = rx.recv().await {
        // TODO: Integrate with rbuilder to submit the bid
        // For now, just log it
        tracing::debug!(
            "📤 COUNTER-BID READY: slot={} block={} value={:.6} ETH via {} - {}",
            counter_bid.slot,
            counter_bid.block_number,
            counter_bid.bid_value_eth,
            counter_bid.relay,
            counter_bid.reason
        );
    }
}

/// Public function to get a counter-bid receiver for external crates
/// Use this to integrate with rbuilder
pub fn get_counter_bid_channel(
    config: crate::config::BiddingConfig,
) -> (Arc<BiddingStrategy>, CounterBidReceiver) {
    create_strategy(config)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("reactive_bidding_service=info".parse().unwrap())
                .add_directive("bid_scraper=info".parse().unwrap()),
        )
        .init();

    // Load configuration
    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    let config = load_config(&config_path)?;

    // Log startup info
    info!("╔═══════════════════════════════════════════════════════════════");
    info!("║ 🚀 Reactive Bidding Service");
    info!("╠═══════════════════════════════════════════════════════════════");
    info!("║ Relays:         {}", config.relays.len());
    info!("║ Debug mode:     {}", config.debug);
    info!("║ Increment:      {:.6} ETH", config.bidding.increment_eth);
    info!("║ Max bid:        {:.4} ETH", config.bidding.max_bid_eth);
    info!("║ Min bid:        {:.6} ETH", config.bidding.min_bid_eth);
    info!("║ Our builders:   {}", config.bidding.our_builders.len());
    info!("║ Whitelisted:    {}", config.bidding.whitelisted_builders.len());
    if let Some(ref path) = config.export_path {
        info!("║ Export path:    {}", path);
    }
    info!("╚═══════════════════════════════════════════════════════════════");

    // Create broadcast channel for bids
    let (bid_tx, bid_rx) = broadcast::channel::<ScrapedRelayBlockBid>(1024);

    // Create the bid sender
    let sender: Arc<dyn BidSender> = Arc::new(BroadcastBidSender::new(bid_tx));

    // Convert config and start bid scraper
    let publishers = convert_relay_config(&config.relays);
    let cancel = CancellationToken::new();

    // Start the bid scraper
    bid_scraper::bid_scraper::run(publishers, sender, cancel.clone());

    // Create recorder (only if debug mode)
    let recorder = create_recorder(config.debug, config.export_path.map(PathBuf::from));

    // Create bidding strategy - always active, sends counter-bids through channel
    let (strategy, counter_bid_rx) = create_strategy(config.bidding);

    // Spawn bid processor
    let processor = tokio::spawn(process_bids(bid_rx, recorder, strategy));

    // Spawn counter-bid handler
    let counter_bid_handler = tokio::spawn(handle_counter_bids(counter_bid_rx));

    // Wait for Ctrl+C
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
            cancel.cancel();
        }
    }

    processor.abort();
    counter_bid_handler.abort();
    info!("Shutdown complete");
    Ok(())
}
