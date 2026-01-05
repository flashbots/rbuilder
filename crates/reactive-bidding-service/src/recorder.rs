//! Block recording module for debugging and analysis.
//!
//! This module tracks all bids per slot and exports detailed statistics
//! for offline analysis. Only active when debug mode is enabled.

use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

use alloy_primitives::U256;
use bid_scraper::types::ScrapedRelayBlockBid;
use parking_lot::Mutex;
use serde::Serialize;
use tracing::info;

/// A single bid point with timing information
#[derive(Debug, Clone, Serialize)]
pub struct BidPoint {
    pub timestamp_ms: u64,
    #[serde(serialize_with = "serialize_u256")]
    pub value: U256,
    pub value_eth: f64,
    pub relay: String,
    pub builder: Option<String>,
}

fn serialize_u256<S>(val: &U256, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&val.to_string())
}

/// Exported auction data for analysis
#[derive(Debug, Clone, Serialize)]
pub struct SlotAuctionExport {
    pub slot: u64,
    pub block_number: u64,
    pub duration_ms: u64,
    pub total_bids: usize,
    pub unique_builders: usize,
    pub unique_relays: usize,
    pub winning_value_eth: f64,
    pub winning_builder: Option<String>,
    pub winning_relay: String,
    pub min_value_eth: f64,
    pub max_value_eth: f64,
    pub price_range_eth: f64,
    pub bids: Vec<BidPoint>,
}

/// Tracks all bids for a single slot
#[derive(Debug, Clone)]
pub struct SlotAuction {
    pub slot: u64,
    pub block_number: u64,
    pub start_time: Instant,
    pub bids: Vec<BidPoint>,
}

impl SlotAuction {
    pub fn new(slot: u64, block_number: u64) -> Self {
        Self {
            slot,
            block_number,
            start_time: Instant::now(),
            bids: Vec::new(),
        }
    }

    pub fn add_bid(&mut self, bid: &ScrapedRelayBlockBid) {
        let elapsed_ms = self.start_time.elapsed().as_millis() as u64;
        let value_eth = bid.value.to_string().parse::<f64>().unwrap_or(0.0) / 1e18;
        self.bids.push(BidPoint {
            timestamp_ms: elapsed_ms,
            value: bid.value,
            value_eth,
            relay: bid.relay_name.clone(),
            builder: bid.builder_pubkey.map(|b| format!("0x{}", b)),
        });
    }

    pub fn best_bid(&self) -> Option<&BidPoint> {
        self.bids.iter().max_by_key(|b| b.value)
    }

    pub fn to_export(&self) -> SlotAuctionExport {
        use std::collections::HashSet;

        let best = self.best_bid();
        let duration_ms = self.bids.last().map(|b| b.timestamp_ms).unwrap_or(0);
        let unique_builders: HashSet<_> = self.bids.iter().filter_map(|b| b.builder.as_ref()).collect();
        let unique_relays: HashSet<_> = self.bids.iter().map(|b| &b.relay).collect();

        let min_val = self.bids.iter().map(|b| b.value_eth).fold(f64::INFINITY, f64::min);
        let max_val = self.bids.iter().map(|b| b.value_eth).fold(f64::NEG_INFINITY, f64::max);

        SlotAuctionExport {
            slot: self.slot,
            block_number: self.block_number,
            duration_ms,
            total_bids: self.bids.len(),
            unique_builders: unique_builders.len(),
            unique_relays: unique_relays.len(),
            winning_value_eth: best.map(|b| b.value_eth).unwrap_or(0.0),
            winning_builder: best.and_then(|b| b.builder.clone()),
            winning_relay: best.map(|b| b.relay.clone()).unwrap_or_default(),
            min_value_eth: if min_val.is_finite() { min_val } else { 0.0 },
            max_value_eth: if max_val.is_finite() { max_val } else { 0.0 },
            price_range_eth: if max_val.is_finite() && min_val.is_finite() { max_val - min_val } else { 0.0 },
            bids: self.bids.clone(),
        }
    }
}

/// Block recorder that tracks auctions and exports data
pub struct BlockRecorder {
    current_auction: Mutex<Option<SlotAuction>>,
    export_path: Option<PathBuf>,
}

impl BlockRecorder {
    pub fn new(export_path: Option<PathBuf>) -> Self {
        Self {
            current_auction: Mutex::new(None),
            export_path,
        }
    }

    /// Process a new bid. Returns the finished auction if we moved to a new block.
    pub fn record_bid(&self, bid: &ScrapedRelayBlockBid) -> Option<SlotAuction> {
        let mut current = self.current_auction.lock();

        // Check if we moved to a new block
        let finished_auction = match current.as_ref() {
            Some(auction) if bid.block_number > auction.block_number => current.take(),
            _ => None,
        };

        // Add bid to current or new auction
        match current.as_mut() {
            Some(auction) if auction.block_number == bid.block_number => {
                auction.add_bid(bid);
            }
            _ => {
                let mut new_auction = SlotAuction::new(bid.slot_number, bid.block_number);
                new_auction.add_bid(bid);
                *current = Some(new_auction);
            }
        }

        // Export and print finished auction
        if let Some(ref auction) = finished_auction {
            self.export_auction(auction);
            self.print_auction_summary(auction);
        }

        finished_auction
    }

    fn export_auction(&self, auction: &SlotAuction) {
        if let Some(ref path) = self.export_path {
            if let Err(e) = self.write_auction_json(auction, path) {
                tracing::error!("Failed to export auction: {}", e);
            }
        }
    }

    fn write_auction_json(&self, auction: &SlotAuction, path: &PathBuf) -> eyre::Result<()> {
        let export = auction.to_export();
        let json = serde_json::to_string(&export)?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        writeln!(file, "{}", json)?;
        Ok(())
    }

    fn print_auction_summary(&self, auction: &SlotAuction) {
        if auction.bids.is_empty() {
            return;
        }

        let best = auction.best_bid().unwrap();
        let builder_short = best
            .builder
            .as_ref()
            .map(|b| format!("{}...{}", &b[..8], &b[b.len() - 4..]))
            .unwrap_or_else(|| "unknown".to_string());

        info!("");
        info!("╔═══════════════════════════════════════════════════════════════════════════════");
        info!("║ 📊 SLOT {} AUCTION (Block {})", auction.slot, auction.block_number);
        info!("╠═══════════════════════════════════════════════════════════════════════════════");
        info!(
            "║ Total bids: {} | Duration: {}ms",
            auction.bids.len(),
            auction.bids.last().map(|b| b.timestamp_ms).unwrap_or(0)
        );
        info!(
            "║ Winner: {} @ {:.6} ETH via {}",
            builder_short,
            best.value_eth,
            best.relay
        );
        info!("╚═══════════════════════════════════════════════════════════════════════════════");
        info!("");
    }
}

/// Create a recorder if debug mode is enabled
pub fn create_recorder(debug: bool, export_path: Option<PathBuf>) -> Option<Arc<BlockRecorder>> {
    if debug {
        Some(Arc::new(BlockRecorder::new(export_path)))
    } else {
        None
    }
}
