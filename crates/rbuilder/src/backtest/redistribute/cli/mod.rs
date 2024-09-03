mod csv_output;

use crate::backtest::redistribute::{calc_redistributions, RedistributionBlockOutput};
use crate::backtest::BlockData;
use crate::live_builder::base_config::load_config_toml_and_env;
use crate::live_builder::cli::LiveBuilderConfig;
use crate::{backtest::HistoricalDataStorage, live_builder::config::Config};
use alloy_primitives::utils::format_ether;
use clap::Parser;
use csv_output::{CSVOutputRow, CSVResultWriter};
use reth_db::DatabaseEnv;
use reth_provider::ProviderFactory;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

#[derive(Parser, Debug)]
struct Cli {
    #[clap(long, help = "Config file path", env = "RBUILDER_CONFIG")]
    config: PathBuf,
    #[clap(long, help = "CSV output path")]
    csv: Option<PathBuf>,
    #[clap(long, help = "JSON output path")]
    json: Option<PathBuf>,
    #[clap(long, help = "distribute to mempool txs", default_value = "false")]
    distribute_to_mempool_txs: bool,
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Parser, Debug)]
enum Commands {
    #[clap(about = "Calculate redistribution of some specific block")]
    Block {
        #[clap(help = "Block number")]
        block_number: u64,
    },
    #[clap(about = "Calculate redistributions in the block range")]
    Range {
        #[clap(help = "Start block number")]
        start_block: u64,
        #[clap(help = "End block number")]
        end_block: u64,
    },
}

pub async fn run_backtest_redistribute<ConfigType: LiveBuilderConfig>() -> eyre::Result<()> {
    let cli = Cli::parse();

    let config: Config = load_config_toml_and_env(cli.config)?;
    config.base_config.setup_tracing_subsriber()?;

    let mut historical_data_storage =
        HistoricalDataStorage::new_from_path(&config.base_config.backtest_fetch_output_file)
            .await?;
    let provider_factory = config
        .base_config
        .provider_factory()?
        .provider_factory_unchecked();
    let mut csv_writer = cli
        .csv
        .map(|path| -> io::Result<_> { CSVResultWriter::new(path) })
        .transpose()?;

    let mut json_accum = cli.json.as_ref().map(|_| Vec::new());
    match cli.command {
        Commands::Block { block_number } => {
            let block_data = historical_data_storage
                .read_block_data(block_number)
                .await?;

            process_redisribution(
                block_data,
                csv_writer.as_mut(),
                json_accum.as_mut(),
                provider_factory.clone(),
                &config,
                cli.distribute_to_mempool_txs,
            )?;
        }
        Commands::Range {
            start_block,
            end_block,
        } => {
            let blocks = (start_block..=end_block).collect::<Vec<_>>();
            let blocks_data = historical_data_storage.read_blocks(&blocks).await?;

            for block_data in blocks_data {
                process_redisribution(
                    block_data,
                    csv_writer.as_mut(),
                    json_accum.as_mut(),
                    provider_factory.clone(),
                    &config,
                    cli.distribute_to_mempool_txs,
                )?;
            }
        }
    }

    if let Some(json_file_path) = cli.json {
        let json_file = std::fs::File::create(json_file_path)?;
        serde_json::to_writer_pretty(json_file, &json_accum.unwrap())?;
    }

    Ok(())
}

fn process_redisribution<ConfigType: LiveBuilderConfig + Send + Sync>(
    block_data: BlockData,
    csv_writer: Option<&mut CSVResultWriter>,
    json_accum: Option<&mut Vec<RedistributionBlockOutput>>,
    provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
    config: &ConfigType,
    distribute_to_mempool_txs: bool,
) -> eyre::Result<()> {
    let block_number = block_data.block_number;
    let block_hash = block_data.onchain_block.header.hash;
    info!(block_number, "Calculating redistribution for a block");
    let redistribution_values = match calc_redistributions(
        provider_factory.clone(),
        config,
        block_data,
        distribute_to_mempool_txs,
    ) {
        Ok(ok) => ok,
        Err(err) => {
            return if err.to_string().contains("Included block data not found") {
                Ok(())
            } else {
                Err(err)
            }
        }
    };

    if let Some(json_accum) = json_accum {
        json_accum.push(redistribution_values.clone())
    }

    let old_output = redistribution_values
        .identities
        .iter()
        .filter_map(|id| {
            if id.redistribution_value_received.is_zero() {
                return None;
            }
            Some((id.address, id.redistribution_value_received))
        })
        .collect::<Vec<_>>();

    if let Some(csv_writer) = csv_writer {
        let values = old_output
            .into_iter()
            .map(|(address, value)| CSVOutputRow {
                block_number,
                block_hash,
                address,
                amount: value,
            })
            .collect();
        csv_writer.write_data(values)?;
    } else {
        for (address, value) in old_output {
            println!("{}: {}", address, format_ether(value));
        }
    }

    Ok(())
}
