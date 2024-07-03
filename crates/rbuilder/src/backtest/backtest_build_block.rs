use ahash::HashMap;
use alloy_primitives::utils::format_ether;

use crate::{
    backtest::{
        execute::{backtest_prepare_ctx_for_block, BacktestBlockInput},
        BlockData, HistoricalDataStorage,
    },
    building::builders::BacktestSimulateBlockInput,
    live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig},
    primitives::{Order, OrderId, SimulatedOrder},
    utils::timestamp_as_u64,
};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
struct Cli {
    #[clap(long, help = "Config file path", env = "RBUILDER_CONFIG")]
    config: PathBuf,
    #[clap(
        long,
        help = "build block lag (ms)",
        default_value = "0",
        allow_hyphen_values = true
    )]
    block_building_time_ms: i64,
    #[clap(long, help = "Show all available orders")]
    show_orders: bool,
    #[clap(long, help = "Show order data and top of block simulation results")]
    show_sim: bool,
    #[clap(long, help = "Show missing block txs")]
    show_missing: bool,
    #[clap(long, help = "don't build block")]
    no_block_building: bool,
    #[clap(
        long,
        help = "builders to build block with (see config builders)",
        default_value = "mp-ordering"
    )]
    builders: Vec<String>,
    #[clap(long, help = "use only this orders")]
    only_order_ids: Vec<String>,
    #[clap(help = "Block Number")]
    block: u64,
}

pub async fn run_backtest_build_block<ConfigType: LiveBuilderConfig>() -> eyre::Result<()> {
    let cli = Cli::parse();

    let config: ConfigType = load_config_toml_and_env(cli.config)?;
    config.base_config().setup_tracing_subsriber()?;

    let mut historical_data_storage =
        HistoricalDataStorage::new_from_path(&config.base_config().backtest_fetch_output_file)
            .await?;

    let mut block_data = historical_data_storage.read_block_data(cli.block).await?;

    if !cli.only_order_ids.is_empty() {
        block_data.filter_orders_by_ids(&cli.only_order_ids);
    }
    if cli.block_building_time_ms != 0 {
        block_data.filter_orders_by_block_lag(cli.block_building_time_ms);
    }

    if cli.show_missing {
        show_missing_txs(&block_data);
    }

    println!(
        "Block: {} {:?}",
        block_data.block_number,
        block_data.onchain_block.header.hash.unwrap_or_default()
    );
    println!(
        "bid value: {}",
        format_ether(block_data.winning_bid_trace.value)
    );
    println!(
        "builder pubkey: {:?}",
        block_data.winning_bid_trace.builder_pubkey
    );

    let (orders, order_and_timestamp): (Vec<Order>, HashMap<OrderId, u64>) = block_data
        .available_orders
        .iter()
        .map(|order| (order.order.clone(), (order.order.id(), order.timestamp_ms)))
        .unzip();

    println!("Available orders: {}", orders.len());

    if cli.show_orders {
        print_order_and_timestamp(&order_and_timestamp, &block_data);
    }

    let provider_factory = config
        .base_config()
        .provider_factory()?
        .provider_factory_unchecked();
    let chain_spec = config.base_config().chain_spec()?;
    let sbundle_mergeabe_signers = config.base_config().sbundle_mergeabe_signers();

    let BacktestBlockInput {
        ctx, sim_orders, ..
    } = backtest_prepare_ctx_for_block(
        block_data.clone(),
        provider_factory.clone(),
        chain_spec.clone(),
        cli.block_building_time_ms,
        config.base_config().blocklist()?,
    )?;

    if cli.show_sim {
        print_simulated_orders(&sim_orders, &order_and_timestamp, &block_data);
    }

    if !cli.no_block_building {
        let winning_builder = cli
            .builders
            .iter()
            .filter_map(|builder_name: &String| {
                let input = BacktestSimulateBlockInput {
                    ctx: ctx.clone(),
                    builder_name: builder_name.clone(),
                    sbundle_mergeabe_signers: sbundle_mergeabe_signers.clone(),
                    sim_orders: &sim_orders,
                    provider_factory: provider_factory.clone(),
                    cached_reads: None,
                };
                let build_res = config.build_backtest_block(builder_name, input);
                if let Err(err) = &build_res {
                    println!("Error building block: {:?}", err);
                    return None;
                }
                let (block, _) = build_res.ok()?;
                println!("Built block {} with builder: {:?}", cli.block, builder_name);
                println!("Builder profit: {}", format_ether(block.trace.bid_value));
                println!(
                    "Number of used orders: {}",
                    block.trace.included_orders.len()
                );

                println!("Used orders:");
                for order_result in &block.trace.included_orders {
                    println!(
                        "{:>74} gas: {:>8} profit: {}",
                        order_result.order.id().to_string(),
                        order_result.gas_used,
                        format_ether(order_result.coinbase_profit),
                    );
                    if let Order::Bundle(_) | Order::ShareBundle(_) = order_result.order {
                        for tx in &order_result.txs {
                            println!("      ↳ {:?}", tx.hash());
                        }

                        for (to, value) in &order_result.paid_kickbacks {
                            println!(
                                "      - kickback to: {:?} value: {}",
                                to,
                                format_ether(*value)
                            );
                        }
                    }
                }
                Some((builder_name.clone(), block.trace.bid_value))
            })
            .max_by_key(|(_, value)| *value);

        if let Some((builder_name, value)) = winning_builder {
            println!(
                "Winning builder: {} with profit: {}",
                builder_name,
                format_ether(value)
            );
        }
    }

    Ok(())
}

/// Convert a timestamp in milliseconds to the slot time relative to the given block timestamp.
fn timestamp_ms_to_slot_time(timestamp_ms: u64, block_timestamp: u64) -> i64 {
    (block_timestamp * 1000) as i64 - (timestamp_ms as i64)
}

/// Print the available orders sorted by timestamp.
fn print_order_and_timestamp(order_and_timestamp: &HashMap<OrderId, u64>, block_data: &BlockData) {
    let mut order_by_ts = order_and_timestamp.clone().into_iter().collect::<Vec<_>>();
    order_by_ts.sort_by_key(|(_, ts)| *ts);
    for (id, ts) in order_by_ts {
        println!(
            "{:>74} ts: {}",
            id.to_string(),
            timestamp_ms_to_slot_time(ts, timestamp_as_u64(&block_data.onchain_block))
        );
    }
}

/// Print information about transactions included on-chain but which are missing in our available orders.
fn show_missing_txs(block_data: &BlockData) {
    let missing_txs = block_data.search_missing_txs_on_available_orders();
    if !missing_txs.is_empty() {
        println!(
            "{} of txs by hashes missing on available orders",
            missing_txs.len()
        );
        for missing_tx in missing_txs.iter() {
            println!("Tx: {:?}", missing_tx);
        }
    }
    let missing_nonce_txs = block_data.search_missing_account_nonce_on_available_orders();
    if !missing_nonce_txs.is_empty() {
        println!(
            "\n{} of txs by nonce pairs missing on available orders",
            missing_nonce_txs.len()
        );
        for missing_nonce_tx in missing_nonce_txs.iter() {
            println!(
                "Tx: {:?}, Account: {:?}, Nonce: {}",
                missing_nonce_tx.0, missing_nonce_tx.1.account, missing_nonce_tx.1.nonce,
            );
        }
    }
}

/// Print information about simulated orders.
fn print_simulated_orders(
    sim_orders: &[SimulatedOrder],
    order_and_timestamp: &HashMap<OrderId, u64>,
    block_data: &BlockData,
) {
    println!("Simulated orders: ({} total)", sim_orders.len());
    let mut sorted_orders = sim_orders.to_owned();
    sorted_orders.sort_by_key(|order| order.sim_value.coinbase_profit);
    sorted_orders.reverse();
    for order in sorted_orders {
        let order_timestamp = order_and_timestamp
            .get(&order.order.id())
            .copied()
            .unwrap_or_default();

        let slot_time_ms =
            timestamp_ms_to_slot_time(order_timestamp, timestamp_as_u64(&block_data.onchain_block));

        println!(
            "{:>74} slot_time_ms: {:>8}, gas: {:>8} profit: {}, parent: {}",
            order.order.id().to_string(),
            slot_time_ms,
            order.sim_value.gas_used,
            format_ether(order.sim_value.coinbase_profit),
            order
                .prev_order
                .map(|prev_order| prev_order.to_string())
                .unwrap_or_else(String::new)
        );
    }
    println!();
}
