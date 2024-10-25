//! Application to fetch orders from different sources (eg: mempool dumpster, external bundles db) and store them on a SQLite DB
//! to be used later (eg: backtest-build-block, backtest-build-range)

use alloy_primitives::utils::format_ether;
use clap::Parser;
use rbuilder::{
    backtest::{HistoricalDataFetcher, HistoricalDataStorage},
    live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig, config::Config},
};
use std::{ffi::OsString, fs, path::PathBuf};
use tracing::error;

#[derive(Parser, Debug)]
struct Cli {
    #[clap(long, help = "Config file path", env = "RBUILDER_CONFIG")]
    config: PathBuf,
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Parser, Debug)]
enum Commands {
    #[clap(about = "Fetch historical data for a list of blocks")]
    Fetch(FetchCommand),
    #[clap(about = "List blocks available in the historical data")]
    List,
    #[clap(about = "Remove blocks from the historical data storage")]
    Remove {
        #[clap(help = "Blocks")]
        blocks: Vec<u64>,
    },
}

#[derive(Parser, Debug)]
struct FetchCommand {
    #[clap(
        long,
        action,
        help = "Ignore errors when fetching blocks, skip blocks with errors"
    )]
    ignore_errors: bool,
    #[clap(
        long,
        action,
        help = "Fetch all blocks in the provided range (blocks argument becomes range)"
    )]
    range: bool,
    #[clap(help = "Blocks")]
    blocks: Vec<u64>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    let config: Config = load_config_toml_and_env(cli.config)?;
    config.base_config().setup_tracing_subscriber()?;

    match cli.command {
        Commands::Fetch(cli) => {
            // create paths for backtest_fetch_mempool_data_dir (i.e "~/.rbuilder/mempool-data" and ".../transactions")
            let backtest_fetch_mempool_data_dir =
                config.base_config().backtest_fetch_mempool_data_dir()?;

            let db = config.base_config().flashbots_db().await?;
            let provider = config.base_config().eth_rpc_provider()?;
            let fetcher = HistoricalDataFetcher::new(
                provider,
                config.base_config().backtest_fetch_eth_rpc_parallel,
            )
            .with_default_datasource(backtest_fetch_mempool_data_dir, db)?;

            let blocks_to_fetch: Box<dyn Iterator<Item = u64>> = if cli.range {
                let from_block = cli.blocks.first().copied().unwrap_or(0);
                let to_block = cli.blocks.last().copied().unwrap_or(0);
                Box::new(from_block..=to_block)
            } else {
                Box::new(cli.blocks.into_iter())
            };

            // expand path for backtest_fetch_output_file and check for sqlite filename (i.e "~/.rbuilder/backtest/main.sqlite")
            let backtest_fetch_output_file = shellexpand::tilde(
                config
                    .base_config()
                    .backtest_fetch_output_file
                    .to_str()
                    .unwrap(),
            );
            let backtest_fetch_output_file_buf =
                PathBuf::from(OsString::from(backtest_fetch_output_file.as_ref()));
            if backtest_fetch_output_file_buf.extension().unwrap() != "sqlite" {
                return Err(eyre::eyre!("Filename must be a sqlite file"));
            }

            // create directory if needed
            let mut backtest_fetch_output_file_path = backtest_fetch_output_file_buf.clone();
            backtest_fetch_output_file_path.pop();
            fs::create_dir_all(&backtest_fetch_output_file_path)?;

            let mut historical_data_storage =
                HistoricalDataStorage::new_from_path(backtest_fetch_output_file_buf).await?;

            for block in blocks_to_fetch {
                let block_data = match fetcher.fetch_historical_data(block).await {
                    Ok(block_data) => block_data,
                    Err(err) => {
                        error!("Failed to fetch block: {}, error: {}", block, err);
                        if cli.ignore_errors {
                            continue;
                        } else {
                            return Err(err);
                        }
                    }
                };
                historical_data_storage.write_block_data(block_data).await?;
            }
        }
        Commands::List => {
            let mut historical_data_storage = HistoricalDataStorage::new_from_path(
                config.base_config().backtest_fetch_output_file.clone(),
            )
            .await?;
            let blocks = historical_data_storage.get_blocks_info().await?;
            println!("block_number,block_hash,order_count,profit");
            for block in blocks {
                println!(
                    "{},{:?},{},{}",
                    block.block_number,
                    block.block_hash,
                    block.order_count,
                    format_ether(block.bid_value),
                );
            }
        }
        Commands::Remove { blocks } => {
            let mut historical_data_storage = HistoricalDataStorage::new_from_path(
                config.base_config().backtest_fetch_output_file.clone(),
            )
            .await?;
            for block in blocks {
                historical_data_storage.remove_block(block).await?;
            }
        }
    }

    Ok(())
}
