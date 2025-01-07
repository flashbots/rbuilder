//! Backtest app to build a multiple blocks in a similar way as we do in live.
//! It gets the orders from a HistoricalDataStorage, simulates the orders and the run the building algorithms.
//! We count the amount of blocks that generated more profit than the landed block ("won" blocks) and we report:
//! - Win %: % of blocks "won"
//! - Total profits: the sum of the profit (= our_true_block_value - landed_bid) for the blocks we won.
//!   This represents how much extra profit we did compared to the landed blocks.
//!   Optionally (via --store-backtest) it can store the simulated results on a SQLite db (config.backtest_results_store_path)
//!   Optionally (via --compare-backtest) it can compare the simulations against previously stored simulations (via --store-backtest)
//!
//! Sample call (numbers are from_block , to_block (inclusive)):
//! - simple backtest: backtest-build-range --config /home/happy_programmer/config.toml 19380913 193809100
//! - backtest storing simulations : backtest-build-range --config /home/happy_programmer/config.toml --store-backtest 19380913 193809100
//! - backtest comparing simulations : backtest-build-range --config /home/happy_programmer/config.toml --compare-backtest 19380913 193809100

use crate::{
    backtest::{
        execute::{backtest_simulate_block, BlockBacktestValue},
        BacktestResultsStorage, BlockData, HistoricalDataStorage, StoredBacktestResult,
    },
    live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig},
};
use alloy_primitives::{utils::format_ether, Address, U256};
use clap::Parser;
use rayon::prelude::*;
use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
};
use time::format_description::well_known::Rfc3339;
use tokio::{signal::ctrl_c, sync::mpsc};
use tokio_util::sync::CancellationToken;
use tracing::warn;

#[derive(Parser, Debug, Clone)]
struct Cli {
    #[clap(long, help = "Config file path", env = "RBUILDER_CONFIG")]
    config: PathBuf,
    #[clap(
        long,
        action,
        help = "Build all blocks in the provided range (blocks argument becomes range)"
    )]
    range: bool,
    #[clap(long, help = "build block lag (ms)", default_value = "0")]
    build_block_lag_ms: u64,
    #[clap(long, help = "Filter cancellable bundles")]
    filter_cancel: bool,
    #[clap(
        long,
        action,
        help = "Store backtest result values in the db for further comparison."
    )]
    store_backtest: bool,
    #[clap(long, action, help = "Compare backtest to the latest stored backtest.")]
    compare_backtest: bool,
    #[clap(long, help = "Path to csv file to write output to")]
    csv: Option<PathBuf>,
    #[clap(long, help = "Ignored signers")]
    ignored_signers: Vec<Address>,
    #[clap(help = "Blocks")]
    blocks: Vec<u64>,
}

pub async fn run_backtest_build_range<ConfigType>() -> eyre::Result<()>
where
    ConfigType: LiveBuilderConfig,
{
    let cli = Cli::parse();

    if cli.store_backtest && cli.compare_backtest {
        return Err(eyre::eyre!(
            "Cannot store and compare backtest results at the same time."
        ));
    }
    let config: ConfigType = load_config_toml_and_env(cli.config.clone())?;
    config.base_config().setup_tracing_subscriber()?;

    let builders_names = config.base_config().backtest_builders.clone();

    let mut historical_data_storage =
        HistoricalDataStorage::new_from_path(&config.base_config().backtest_fetch_output_file)
            .await?;
    let mut backtest_results_storage =
        BacktestResultsStorage::new_from_path(&config.base_config().backtest_results_store_path)
            .await?;

    let blocks = {
        let mut result = Vec::new();
        let available_blocks = historical_data_storage.get_blocks().await?;
        if !cli.blocks.is_empty() {
            if cli.range {
                let from_block = cli.blocks.iter().min().copied().unwrap_or(0);
                let to_block = cli.blocks.iter().max().copied().unwrap_or(0);
                for block in available_blocks {
                    if from_block <= block && block <= to_block {
                        result.push(block);
                    }
                }
            } else {
                for block in available_blocks {
                    if cli.blocks.contains(&block) {
                        result.push(block);
                    }
                }
            };
        } else {
            result = available_blocks.into_iter().collect();
        }
        result.sort();
        result
    };

    let provider_factory = config.base_config().create_provider_factory()?;
    let chain_spec = config.base_config().chain_spec()?;

    let mut profits = Vec::new();
    let mut losses = Vec::new();

    let cancel_token = CancellationToken::new();

    let cancel_token_clone = cancel_token.clone();
    tokio::spawn(async move {
        ctrl_c().await.unwrap_or_default();
        cancel_token_clone.cancel();
    });

    let mut csv_output = if let Some(file) = cli.csv {
        let mut csv_output = CSVResultWriter::new(file, builders_names.clone())?;
        csv_output.write_header()?;
        Some(csv_output)
    } else {
        None
    };

    let blocklist = config.base_config().blocklist()?;

    let mut read_blocks = spawn_block_fetcher(
        historical_data_storage,
        blocks.clone(),
        cli.ignored_signers,
        cancel_token.clone(),
    );

    loop {
        if cancel_token.is_cancelled() {
            break;
        }
        let blocks = if let Some(blocks) = read_blocks.recv().await {
            blocks
        } else {
            break;
        };
        // process the read blocks to get the BlockBacktestValues
        let input = blocks
            .into_iter()
            .map(|block_data| {
                (
                    block_data,
                    cli.build_block_lag_ms,
                    provider_factory.clone(),
                    chain_spec.clone(),
                    builders_names.clone(),
                    blocklist.clone(),
                )
            })
            .collect::<Vec<_>>();
        let output = input
            .into_par_iter()
            .filter_map(
                |(block_data, lag, provider_factory, chain_spec, builders_names, blocklist)| {
                    let block_number = block_data.block_number;
                    match backtest_simulate_block(
                        block_data,
                        provider_factory,
                        chain_spec,
                        lag as i64,
                        builders_names,
                        &config,
                        blocklist,
                        &config.base_config().sbundle_mergeabe_signers(),
                    ) {
                        Ok(ok) => Some(ok),
                        Err(err) => {
                            warn!(
                                "Failed to backtest block, block: {}, err: {:?}",
                                block_number, err
                            );
                            None
                        }
                    }
                },
            )
            .collect::<Vec<_>>();

        // Compare the BlockBacktestValues with the landed block and optionally compare or store
        for o in output {
            if let Some(csv_output) = &mut csv_output {
                csv_output.write_block_data(&o)?;
            }

            let our = o
                .builder_outputs
                .iter()
                .map(|o| o.our_bid_value)
                .max()
                .unwrap_or_default();
            let win = o.winning_bid_value;

            if our > win {
                profits.push(our - win);
            } else {
                losses.push(win - our);
            }

            if cli.compare_backtest {
                let stored_result = if let Some(res) = backtest_results_storage
                    .load_latest_backtest_result(o.block_number)
                    .await?
                {
                    res
                } else {
                    continue;
                };
                print_backtest_value_diff(&stored_result, &o);
            } else if cli.store_backtest {
                backtest_results_storage
                    .store_backtest_results(time::OffsetDateTime::now_utc(), &[o.clone()])
                    .await?;
                print_backtest_value(o);
            } else {
                print_backtest_value(o);
            }
        }
    }

    println!(
        "Win %: {}",
        (profits.len() as f64 / (profits.len() + losses.len()) as f64) * 100.0
    );
    println!(
        "Total profits: {}",
        format_ether(profits.iter().sum::<U256>())
    );

    Ok(())
}

fn print_backtest_value(mut output: BlockBacktestValue) {
    output.builder_outputs.sort_by(|a, b| {
        b.our_bid_value
            .cmp(&a.our_bid_value)
            .then(b.builder_name.cmp(&a.builder_name))
    });
    println!("block:       {}", output.block_number);
    println!("bid_val:     {}", format_ether(output.winning_bid_value));
    if let Some(best_b) = output.builder_outputs.first() {
        println!(
            "best_bldr:   {} {} {}",
            format_ether(best_b.our_bid_value),
            best_b.orders_included,
            best_b.builder_name
        );
    }
    println!("won_by:      {}", output.extra_data);
    println!("sim_ord:     {}", output.simulated_orders_count);
    println!("sim_blocked: {}", output.filtered_orders_blocklist_count);
    println!("sim_n_ref:   {}", output.simulated_orders_with_refund);
    println!(
        "sim_sum_ref: {}",
        format_ether(output.simulated_refunds_paid)
    );
    for b in output.builder_outputs {
        println!(
            "  bldr:  {} {} {}",
            format_ether(b.our_bid_value),
            b.orders_included,
            b.builder_name
        );
    }

    println!()
}

fn print_backtest_value_diff(stored: &StoredBacktestResult, output: &BlockBacktestValue) {
    println!(
        "comparing block {}, to: {} {}",
        stored.backtest_result.block_number,
        stored.rbuilder_version,
        stored.time.format(&Rfc3339).unwrap()
    );
    let stored = &stored.backtest_result;
    if stored.simulated_orders_count != output.simulated_orders_count {
        println!(
            "diff in simulated_orders_count: {} -> {}",
            stored.simulated_orders_count, output.simulated_orders_count
        );
    }
    if stored.simulated_total_gas != output.simulated_total_gas {
        println!(
            "diff in simulated_total_gas: {} -> {}",
            stored.simulated_total_gas, output.simulated_total_gas
        );
    }
    if stored.filtered_orders_blocklist_count != output.filtered_orders_blocklist_count {
        println!(
            "diff in filtered_orders_blocklist_count: {} -> {}",
            stored.filtered_orders_blocklist_count, output.filtered_orders_blocklist_count
        );
    }
    if stored.simulated_orders_with_refund != output.simulated_orders_with_refund {
        println!(
            "diff in simulated_orders_with_refund: {} -> {}",
            stored.simulated_orders_with_refund, output.simulated_orders_with_refund
        );
    }
    if stored.simulated_refunds_paid != output.simulated_refunds_paid {
        println!(
            "diff in simulated_refunds_paid: {} -> {}",
            format_ether(stored.simulated_refunds_paid),
            format_ether(output.simulated_refunds_paid)
        );
    }
    let max_profit_stored = stored
        .builder_outputs
        .iter()
        .map(|o| o.our_bid_value)
        .max()
        .unwrap_or_default();
    let profit_stored = output
        .builder_outputs
        .iter()
        .map(|o| o.our_bid_value)
        .max()
        .unwrap_or_default();
    if max_profit_stored != profit_stored {
        println!(
            "diff in max profit: {} -> {}",
            format_ether(max_profit_stored),
            format_ether(profit_stored)
        );
    }
    println!()
}

#[derive(Debug)]
struct CSVResultWriter {
    file: File,
    builder_names: Vec<String>,
}

impl CSVResultWriter {
    fn new(path: impl AsRef<Path>, builder_names: Vec<String>) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            file,
            builder_names,
        })
    }

    fn write_header(&mut self) -> io::Result<()> {
        let mut line = String::new();
        line.push_str("block_number,winning_bid_value,simulated_orders_count");
        for builder_name in &self.builder_names {
            line.push_str(&format!(",{}", builder_name));
        }
        writeln!(self.file, "{}", line)?;
        self.file.flush()
    }

    fn write_block_data(&mut self, value: &BlockBacktestValue) -> io::Result<()> {
        let mut line = String::new();
        line.push_str(&format!(
            "{},{},{}",
            value.block_number,
            format_ether(value.winning_bid_value),
            value.simulated_orders_count
        ));
        for builder in &self.builder_names {
            let builder_res = value
                .builder_outputs
                .iter()
                .find(|b| b.builder_name == *builder)
                .map(|b| b.our_bid_value)
                .unwrap_or_default();
            line.push_str(&format!(",{}", format_ether(builder_res)));
        }
        writeln!(self.file, "{}", line)?;
        self.file.flush()
    }
}

/// Spawns a task that reads BlockData from the HistoricalDataStorage in blocks of current_num_threads.
/// The results can the be polled from the returned mpsc::Receiver
/// This allows us to process a batch while the next is being fetched.
fn spawn_block_fetcher(
    mut historical_data_storage: HistoricalDataStorage,
    blocks: Vec<u64>,
    ignored_signers: Vec<Address>,
    cancellation_token: CancellationToken,
) -> mpsc::Receiver<Vec<BlockData>> {
    let (sender, receiver) = mpsc::channel(10);

    tokio::spawn(async move {
        for blocks in blocks.chunks(rayon::current_num_threads()) {
            if cancellation_token.is_cancelled() {
                return;
            }
            let mut blocks = match historical_data_storage.read_blocks(blocks).await {
                Ok(res) => res,
                Err(err) => {
                    warn!("Failed to read blocks from storage: {:?}", err);
                    return;
                }
            };
            for block in &mut blocks {
                block.filter_out_ignored_signers(&ignored_signers);
            }
            match sender.send(blocks).await {
                Ok(_) => {}
                Err(err) => {
                    warn!("Failed to send blocks to processing: {:?}", err);
                    return;
                }
            }
        }
    });

    receiver
}
