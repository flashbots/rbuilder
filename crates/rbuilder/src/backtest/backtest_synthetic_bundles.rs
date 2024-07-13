use crate::{
    backtest::{
        execute::{backtest_prepare_ctx_for_block, BacktestBlockInput},
        fetch::{
            csv::CSVDatasource,
            datasource::{BlockRef, DataSource},
        },
        BlockData, HistoricalDataStorage, OrdersWithTimestamp,
    },
    building::{builders::BacktestSimulateBlockInput, sim::simulate_all_orders_with_sim_tree},
    live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig},
    primitives::{Order, OrderId, SimulatedOrder},
    utils::timestamp_as_u64,
};
use alloy_primitives::{utils::format_ether, Address};
use clap::Parser;
use std::collections::HashMap;
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
    #[clap(long, help = "csv file path", env = "rbuilder_csv")]
    csv: PathBuf,
}

const MAX_PAYMENT: u128 = 100_000_000_000_000_000; // 0.1 ETH

pub async fn run_backtest_with_synthetic_bundles<ConfigType: LiveBuilderConfig>() -> eyre::Result<()>
{
    let cli = Cli::parse();

    let config: ConfigType = load_config_toml_and_env(cli.config)?;
    config.base_config().setup_tracing_subsriber()?;

    let mut historical_data_storage =
        HistoricalDataStorage::new_from_path(&config.base_config().backtest_fetch_output_file)
            .await?;

    let block_data = historical_data_storage.read_block_data(cli.block).await?;
    let block_ref = BlockRef::new(cli.block, block_data.onchain_block.header.timestamp);

    let csv_datasource = CSVDatasource::new(cli.csv)?;
    let available_orders = csv_datasource.get_orders(block_ref).await?;

    println!("Loaded {} orders", available_orders.len());
    let addresses = get_addresses(&available_orders);

    let balances_to_increase = addresses
        .iter()
        .map(|address| (*address, MAX_PAYMENT * 10))
        .collect::<Vec<(Address, u128)>>();

    println!("Available orders: {:?}", available_orders.len());

    let provider_factory = config
        .base_config()
        .provider_factory()?
        .provider_factory_unchecked();
    let chain_spec = config.base_config().chain_spec()?;
    let sbundle_mergeabe_signers = config.base_config().sbundle_mergeabe_signers();

    let BacktestBlockInput { mut ctx, .. } = backtest_prepare_ctx_for_block(
        block_data.clone(),
        provider_factory.clone(),
        chain_spec.clone(),
        cli.block_building_time_ms,
        config.base_config().blocklist()?,
    )?;

    ctx.set_backtest_balances_to_spoof(balances_to_increase);

    let available_orders: Vec<Order> = available_orders
        .into_iter()
        .map(|order_with_ts| order_with_ts.order)
        .collect();

    let (sim_orders, _) = simulate_all_orders_with_sim_tree(
        provider_factory.clone(),
        &ctx,
        &available_orders,
        false,
    )?;

    println!("Simulated orders: {:?}", sim_orders.len());

    let mut order_and_timestamp: HashMap<OrderId, u64> = HashMap::new();
    for order in &sim_orders {
        order_and_timestamp.insert(order.order.id(), block_data.onchain_block.header.timestamp);
    }

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
    };

    Ok(())
}

fn get_addresses(orders: &Vec<OrdersWithTimestamp>) -> Vec<Address> {
    let mut addresses = Vec::new();
    for order in orders {
        let txs = order.order.list_txs();
        for (tx, _) in txs {
            addresses.push(tx.signer());
        }
    }
    addresses
}

/// Convert a timestamp in milliseconds to the slot time relative to the given block timestamp.
fn timestamp_ms_to_slot_time(timestamp_ms: u64, block_timestamp: u64) -> i64 {
    (block_timestamp * 1000) as i64 - (timestamp_ms as i64)
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
