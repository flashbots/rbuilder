//! Backtest app to build a single block in a similar way as we do in live.
//! It gets the orders from a HistoricalDataStorage, simulates the orders and then runs the building algorithms.
//! It outputs the best algorithm (most profit) so we can check for improvements in our [crate::building::builders::BlockBuildingAlgorithm]s
//! BlockBuildingAlgorithm are defined on the config file but selected on the command line via "--builders"
//! Sample call:
//! backtest-build-block --config /home/happy_programmer/config.toml --builders mgp-ordering --builders mp-ordering 19380913 --show-orders --show-missing

use ahash::HashMap;
use alloy_primitives::{utils::format_ether, TxHash};

use crate::{
    backtest::{
        execute::{backtest_prepare_orders_from_building_context, BacktestBlockInput},
        OrdersWithTimestamp,
    },
    building::{
        builders::BacktestSimulateBlockInput, BlockBuildingContext, ExecutionResult,
        NullPartialBlockExecutionTracer,
    },
    live_builder::cli::LiveBuilderConfig,
    provider::StateProviderFactory,
};
use clap::Parser;
use rbuilder_primitives::{order_statistics::OrderStatistics, Order, OrderId, SimulatedOrder};
use std::{path::PathBuf, sync::Arc};

#[derive(Parser, Debug)]
pub struct BuildBlockCfg {
    #[clap(long, help = "Config file path", env = "RBUILDER_CONFIG")]
    pub config: PathBuf,
    #[clap(long, help = "Show all available orders")]
    pub show_orders: bool,
    #[clap(long, help = "Show order data and top of block simulation results")]
    pub show_sim: bool,
    #[clap(long, help = "don't build block")]
    pub no_block_building: bool,
    #[clap(
        long,
        help = "builders to build block with (see config builders)",
        default_value = "mp-ordering"
    )]
    pub builders: Vec<String>,
    #[clap(
        long,
        help = "Traces block building execution (shows all executed orders and txs)"
    )]
    pub trace_block_building: bool,
    #[clap(
        long,
        help = "Shows any order and sim order containing this tx hash. Example: --show-tx-extra-data 0x4905f253e997236afecddb080e38028227b083c4d9921209df7fda192f0ec428"
    )]
    pub show_tx_extra_data: Option<TxHash>,
}

/// Provides all the orders needed to simulate the construction of a block.
/// It also provides the needed context to execute those orders.
pub trait OrdersSource<ConfigType, ProviderType>
where
    ConfigType: LiveBuilderConfig,
    ProviderType: StateProviderFactory + Clone + 'static,
{
    fn config(&self) -> &ConfigType;
    /// Orders available to build blocks with their time of arrival.
    fn available_orders(&self) -> Vec<OrdersWithTimestamp>;
    /// Start of the slot for the block.
    /// Usually all the orders will arrive before block_time_as_unix_ms + 4secs (max get_header time from validator to relays).
    fn block_time_as_unix_ms(&self) -> u64;

    /// ugly: it takes BaseConfig but not all implementations need it.....
    fn create_provider_factory(&self) -> eyre::Result<ProviderType>;

    fn create_block_building_context(&self) -> eyre::Result<BlockBuildingContext>;

    /// Prints any stats specific to the particular OrdersSource implementation (eg: parameters, block simulation)
    fn print_custom_stats(&self, provider: ProviderType) -> eyre::Result<()>;
}

pub async fn run_backtest_build_block<ConfigType, OrdersSourceType, ProviderType>(
    build_block_cfg: BuildBlockCfg,
    orders_source: OrdersSourceType,
) -> eyre::Result<()>
where
    ConfigType: LiveBuilderConfig,
    ProviderType: StateProviderFactory + Clone + 'static,
    OrdersSourceType: OrdersSource<ConfigType, ProviderType>,
{
    let ctx = orders_source.create_block_building_context()?;

    let config = orders_source.config();
    config.base_config().setup_tracing_subscriber()?;

    let available_orders = orders_source.available_orders();
    let mut order_statistics = OrderStatistics::new();
    for order in &available_orders {
        order_statistics.add(&order.order);
    }
    println!("mev_blocker_price {}", format_ether(ctx.mev_blocker_price));
    println!("Available orders: {}", available_orders.len());
    println!("Available orders: {}", available_orders.len());
    println!("Order statistics: {order_statistics:?}");

    let provider_factory = orders_source.create_provider_factory()?;
    orders_source.print_custom_stats(provider_factory.clone())?;

    let BacktestBlockInput { sim_orders, .. } = backtest_prepare_orders_from_building_context(
        ctx.clone(),
        available_orders.clone(),
        provider_factory.clone(),
        &config.base_config().sbundle_mergeable_signers(),
    )?;

    if let Some(tx_hash) = build_block_cfg.show_tx_extra_data {
        print_orders_with_tx_hash(tx_hash, &available_orders, &sim_orders);
    }

    if build_block_cfg.show_orders {
        print_order_and_timestamp(&available_orders, orders_source.block_time_as_unix_ms());
    }

    if build_block_cfg.show_sim {
        let order_and_timestamp: HashMap<OrderId, u64> = available_orders
            .iter()
            .map(|order| (order.order.id(), order.timestamp_ms))
            .collect();
        print_simulated_orders(
            &sim_orders,
            &order_and_timestamp,
            orders_source.block_time_as_unix_ms(),
        );
    }

    if !build_block_cfg.no_block_building {
        let winning_builder = build_block_cfg
            .builders
            .iter()
            .filter_map(|builder_name: &String| {
                let input = BacktestSimulateBlockInput {
                    ctx: ctx.clone(),
                    builder_name: builder_name.clone(),
                    sim_orders: &sim_orders,
                    provider: provider_factory.clone(),
                };
                let build_res = if build_block_cfg.trace_block_building {
                    config.build_backtest_block(
                    builder_name,
                    input,
                    crate::backtest::build_block::full_partial_block_execution_tracer::FullPartialBlockExecutionTracer::new())
                } else {
                    config.build_backtest_block(
                    builder_name,
                    input,
                    NullPartialBlockExecutionTracer{})
                };
                if let Err(err) = &build_res {
                    println!("Error building block: {err:?}");
                    return None;
                }
                let block = build_res.ok()?;
                println!(
                    "Built block {} with builder: {builder_name:?}",
                    ctx.block()
                );
                println!("Builder profit: {}", format_ether(block.trace.bid_value));
                println!(
                    "Number of used orders: {}",
                    block.trace.included_orders.len()
                );
                block.trace.included_orders.iter().for_each(print_order_execution_result);
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

fn print_order(order: &Order) {
    println!("{}", order.id());
    if let Order::Bundle(_) | Order::ShareBundle(_) = order {
        for (tx, _) in order.list_txs() {
            println!("      ↳ {:?}", tx.hash());
        }
    }
}

fn print_sim_order(sim_order: &SimulatedOrder) {
    print_order(&sim_order.order);
    let sim_value = &sim_order.sim_value;
    let profit_info = [
        ("full", sim_value.full_profit_info()),
        ("non_mempool", sim_value.non_mempool_profit_info()),
    ];
    for (name, profit_info) in profit_info {
        println!(
            "      * {name}: coinbase_profit {} mev_gas_price {}",
            format_ether(profit_info.coinbase_profit()),
            format_ether(profit_info.mev_gas_price())
        );
    }
    println!("      * gas_used {:?}", sim_value.gas_used());
}

fn print_orders_with_tx_hash(
    tx_hash: TxHash,
    available_orders: &[OrdersWithTimestamp],
    sim_orders: &[Arc<SimulatedOrder>],
) {
    println!("---- BEGIN Orders with tx hash: {:?}", tx_hash);
    println!("ORDERS:");

    available_orders
        .iter()
        .map(|order_with_timestamp| &order_with_timestamp.order)
        .filter(|order| order.list_txs().iter().any(|(tx, _)| tx.hash() == tx_hash))
        .for_each(print_order);
    println!("\nSIM ORDERS:");
    sim_orders
        .iter()
        .filter(|order| {
            order
                .order
                .list_txs()
                .iter()
                .any(|(tx, _)| tx.hash() == tx_hash)
        })
        .for_each(|sim_order| print_sim_order(sim_order.as_ref()));
    println!("---- END Orders with tx hash: {:?}", tx_hash);
}

fn print_order_execution_result(order_result: &ExecutionResult) {
    println!(
        "{:<74} gas: {:>8} profit: {}",
        order_result.order.id().to_string(),
        order_result.space_used.gas,
        format_ether(order_result.coinbase_profit),
    );
    if let Order::Bundle(_) | Order::ShareBundle(_) = order_result.order {
        for tx in order_result.tx_infos.iter().map(|info| &info.tx) {
            println!("      ↳ {:?}", tx.hash());
        }

        for (to, value) in &order_result.paid_kickbacks {
            println!(
                "      $ Paid kickback to: {:?} value: {}",
                to,
                format_ether(*value)
            );
        }

        if let Some(delayed_kickback) = &order_result.delayed_kickback {
            println!(
                "      $ Delayed kickback to: {:?} value: {} tx_fee: {} paid at end of block: {}",
                delayed_kickback.recipient,
                format_ether(delayed_kickback.payout_value),
                format_ether(delayed_kickback.payout_tx_fee),
                delayed_kickback.should_pay_in_block
            );
        }
    }
}

/// Convert a timestamp in milliseconds to the slot time relative to the given block timestamp.
fn timestamp_ms_to_slot_time(timestamp_ms: u64, block_timestamp: u64) -> i64 {
    (block_timestamp * 1000) as i64 - (timestamp_ms as i64)
}

/// Print the available orders sorted by timestamp.
fn print_order_and_timestamp(orders_with_ts: &[OrdersWithTimestamp], block_time_as_unix_ms: u64) {
    println!("---- BEGIN Orders and timestamp:");
    let mut order_by_ts = orders_with_ts.to_vec();
    order_by_ts.sort_by_key(|owt| owt.timestamp_ms);
    for owt in order_by_ts {
        let id = owt.order.id();
        println!(
            "{:>74} ts: {}",
            id.to_string(),
            timestamp_ms_to_slot_time(owt.timestamp_ms, block_time_as_unix_ms)
        );
        for (tx, optional) in owt.order.list_txs() {
            println!("    {:?} {:?}", tx.hash(), optional);
            println!(
                "        from: {:?} to: {:?} nonce: {}",
                tx.signer(),
                tx.to(),
                tx.nonce()
            )
        }
    }
    println!("---- END Orders and timestamp");
}

/// Print information about simulated orders.
fn print_simulated_orders(
    sim_orders: &[Arc<SimulatedOrder>],
    order_and_timestamp: &HashMap<OrderId, u64>,
    block_time_as_unix_ms: u64,
) {
    println!("Simulated orders: ({} total)", sim_orders.len());
    let mut sorted_orders = sim_orders.to_owned();
    sorted_orders.sort_by_key(|order| order.sim_value.full_profit_info().coinbase_profit());
    sorted_orders.reverse();
    for order in sorted_orders {
        let order_timestamp = order_and_timestamp
            .get(&order.order.id())
            .copied()
            .unwrap_or_default();

        let slot_time_ms = timestamp_ms_to_slot_time(order_timestamp, block_time_as_unix_ms);

        println!(
            "{:>74} slot_time_ms: {:>8}, gas: {:>8} profit: {}",
            order.order.id().to_string(),
            slot_time_ms,
            order.sim_value.gas_used(),
            format_ether(order.sim_value.full_profit_info().coinbase_profit()),
        );
    }
    println!();
}
