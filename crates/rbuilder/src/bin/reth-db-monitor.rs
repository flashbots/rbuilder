//! `reth-db-monitor` - Monitor reth database for block changes
//!
//! This tool connects to a reth database and periodically checks for new blocks,
//! logging changes when they occur.
//!
//! Usage:
//!   `cargo run --bin reth-db-monitor -- --config <path-to-rbuilder-config>`
//!   `cargo run --bin reth-db-monitor -- --reth-path <path-to-reth-datadir> --chain mainnet`

use alloy_primitives::BlockHash;
use clap::Parser;
use eyre::Context;
use rbuilder::{
    live_builder::{
        base_config::{create_provider_factory, BaseConfig},
        get_last_blocks,
    },
    provider::StateProviderFactory,
    utils::ProviderFactoryReopener,
};
use reth::chainspec::chain_value_parser;
use reth_db::DatabaseEnv;
use reth_node_api::NodeTypesWithDBAdapter;
use reth_node_ethereum::EthereumNode;
use serde::Deserialize;
use std::{fs, path::PathBuf, sync::Arc, time::Duration};
use tokio::time;
use tracing::info;

#[derive(Debug, Clone, Parser)]
#[clap(
    name = "reth-db-monitor",
    about = "Monitor reth database for block changes",
    long_about = "Monitor reth database for block changes.\n\n\
                  Usage modes:\n  \
                  1. Using rbuilder config: --config <path>\n  \
                  2. Using reth data dir:   --reth-path <path> [--chain <chain>]"
)]
struct Cli {
    /// Path to the rbuilder config file
    #[arg(long, conflicts_with_all = ["reth_path", "chain"])]
    config: Option<PathBuf>,

    /// Path to reth's data directory
    #[arg(long)]
    reth_path: Option<PathBuf>,

    /// Chain name (e.g., mainnet, sepolia, holesky)
    #[arg(long, default_value = "mainnet")]
    chain: String,

    /// Polling interval in milliseconds
    #[arg(long, default_value = "500")]
    interval_ms: u64,
}

impl Cli {
    fn validate(&self) -> eyre::Result<()> {
        if self.config.is_none() && self.reth_path.is_none() {
            eyre::bail!(
                "Either --config or --reth-path must be provided.\n\n\
                 Usage modes:\n  \
                 1. Using rbuilder config: --config <path>\n  \
                 2. Using reth data dir:   --reth-path <path> [--chain <chain>]\n\n\
                 Run with --help for more information."
            );
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    // Validate CLI arguments
    cli.validate()?;

    // Setup basic logging
    setup_logging();

    info!(interval_ms = cli.interval_ms, "Starting reth-db-monitor");

    // Create provider based on CLI arguments
    let provider = if let Some(config_path) = &cli.config {
        info!(?config_path, "Loading rbuilder config");
        create_provider_from_config(config_path.clone()).await?
    } else {
        // Validation ensures reth_path is present
        let reth_path = cli.reth_path.as_ref().unwrap();
        info!(?reth_path, chain = cli.chain, "Opening reth database");
        create_provider_from_reth_path(reth_path.clone(), &cli.chain)?
    };

    info!("Database connection established");

    // Start monitoring loop
    monitor_blocks(provider, Duration::from_millis(cli.interval_ms)).await?;

    Ok(())
}

/// Setup basic logging to stdout
fn setup_logging() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(env_filter)
        .init();
}

/// Dummy cfg containing only the base config.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ConfigWithBaseConfig {
    #[serde(flatten)]
    pub base_config: BaseConfig,
}

/// Create provider from any config file that contains BaseConfig fields
/// Works with rbuilder, rbuilder-operator, or any config with flattened BaseConfig
async fn create_provider_from_config(
    config_path: PathBuf,
) -> eyre::Result<ProviderFactoryReopener<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>> {
    // Read and parse the config file as BaseConfig
    // This works because BaseConfig uses #[serde(flatten)] in the parent configs
    let config_str = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {:?}", config_path))?;

    let config: ConfigWithBaseConfig = toml::from_str(&config_str).with_context(|| {
        format!(
            "Failed to parse config file as BaseConfig: {:?}",
            config_path
        )
    })?;

    // We don't need root hash computation for monitoring, so we pass true to skip it
    let provider = config
        .base_config
        .create_reth_provider_factory(true) // skip_root_hash = true
        .context("Failed to create provider from config")?;

    Ok(provider)
}

/// Create provider from reth path and chain name
fn create_provider_from_reth_path(
    reth_path: PathBuf,
    chain: &str,
) -> eyre::Result<ProviderFactoryReopener<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>> {
    let chain_spec = chain_value_parser(chain).context("Failed to parse chain name")?;

    // We don't need root hash computation for monitoring, so we pass None
    let provider = create_provider_factory(
        Some(&reth_path),
        None,
        None,
        None,
        chain_spec,
        false, // read-only
        None,  // no root hash
    )
    .context("Failed to create provider from reth path")?;

    Ok(provider)
}

/// Monitor blocks and log when they change
async fn monitor_blocks(
    provider: impl StateProviderFactory,
    interval: Duration,
) -> eyre::Result<()> {
    let mut last_blocks: Vec<(u64, Option<BlockHash>)> = Vec::new();
    let mut interval_ticker = time::interval(interval);

    loop {
        interval_ticker.tick().await;
        let current_blocks = get_last_blocks(&provider).await;
        // Only log if blocks have changed
        if current_blocks != last_blocks {
            info!(?current_blocks, "New blocks");
            last_blocks = current_blocks;
        }
    }
}
