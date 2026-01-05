//! Configuration for the reactive bidding service.

use std::{collections::HashSet, fs, path::PathBuf};

use alloy_primitives::U256;
use eyre::{Context, Result};
use serde::Deserialize;

/// Main configuration for the reactive bidding service
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Relay configurations
    pub relays: Vec<RelayConfig>,

    /// Path to export auction data (JSONL format)
    pub export_path: Option<String>,

    /// Enable debug mode (enables block recording)
    #[serde(default)]
    pub debug: bool,

    /// Bidding strategy configuration
    #[serde(default)]
    pub bidding: BiddingConfig,
}

/// Configuration for a relay connection
#[derive(Debug, Clone, Deserialize)]
pub struct RelayConfig {
    pub name: String,
    pub url: String,
    pub builder_id: Option<String>,
    pub api_token: Option<String>,
}

/// Bidding strategy configuration
#[derive(Debug, Clone, Deserialize)]
pub struct BiddingConfig {
    /// Increment to add on top of the current best bid (in ETH)
    /// Default: 0.0001 ETH (0.1 mETH) - based on observed market data
    #[serde(default = "default_increment")]
    pub increment_eth: f64,

    /// Maximum bid cap (in ETH) - never bid above this
    /// Default: 0.1 ETH
    #[serde(default = "default_max_bid")]
    pub max_bid_eth: f64,

    /// Minimum bid (in ETH) - don't bid if best bid is below this
    /// Default: 0.0001 ETH
    #[serde(default = "default_min_bid")]
    pub min_bid_eth: f64,

    /// Our builder public keys (hex strings with 0x prefix)
    /// We won't outbid ourselves
    #[serde(default)]
    pub our_builders: Vec<String>,

    /// Whitelisted builder public keys (hex strings with 0x prefix)
    /// We won't outbid these builders (e.g., partners)
    #[serde(default)]
    pub whitelisted_builders: Vec<String>,
}

fn default_increment() -> f64 {
    0.0001 // 0.1 mETH - competitive based on observed data
}

fn default_max_bid() -> f64 {
    0.1 // 0.1 ETH
}

fn default_min_bid() -> f64 {
    0.0001 // 0.1 mETH
}

impl Default for BiddingConfig {
    fn default() -> Self {
        Self {
            increment_eth: default_increment(),
            max_bid_eth: default_max_bid(),
            min_bid_eth: default_min_bid(),
            our_builders: Vec::new(),
            whitelisted_builders: Vec::new(),
        }
    }
}

impl BiddingConfig {
    /// Convert increment to wei (U256)
    pub fn increment_wei(&self) -> U256 {
        eth_to_wei(self.increment_eth)
    }

    /// Convert max bid to wei (U256)
    pub fn max_bid_wei(&self) -> U256 {
        eth_to_wei(self.max_bid_eth)
    }

    /// Convert min bid to wei (U256)
    pub fn min_bid_wei(&self) -> U256 {
        eth_to_wei(self.min_bid_eth)
    }

    /// Get set of all builder addresses we should not outbid
    pub fn protected_builders(&self) -> HashSet<String> {
        let mut set = HashSet::new();
        for b in &self.our_builders {
            set.insert(b.to_lowercase());
        }
        for b in &self.whitelisted_builders {
            set.insert(b.to_lowercase());
        }
        set
    }
}

/// Convert ETH to wei
fn eth_to_wei(eth: f64) -> U256 {
    let wei = (eth * 1e18) as u128;
    U256::from(wei)
}

/// Load configuration from a TOML file
pub fn load_config(path: &PathBuf) -> Result<Config> {
    let content = fs::read_to_string(path).wrap_err("Failed to read config file")?;
    toml::from_str(&content).wrap_err("Failed to parse config file")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eth_to_wei() {
        let wei = eth_to_wei(1.0);
        assert_eq!(wei, U256::from(1_000_000_000_000_000_000u128));

        let wei = eth_to_wei(0.0001);
        assert_eq!(wei, U256::from(100_000_000_000_000u128));
    }

    #[test]
    fn test_default_config() {
        let config = BiddingConfig::default();
        assert_eq!(config.increment_eth, 0.0001);
        assert_eq!(config.max_bid_eth, 0.1);
        assert_eq!(config.min_bid_eth, 0.0001);
    }
}
