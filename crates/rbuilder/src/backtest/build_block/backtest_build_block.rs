//! Backtest app to build a single block in a similar way as we do in live.
//! It gets the orders from a HistoricalDataStorage, simulates the orders and then runs the building algorithms.
//! It outputs the best algorithm (most profit) so we can check for improvements in our [crate::building::builders::BlockBuildingAlgorithm]s
//! BlockBuildingAlgorithm are defined on the config file but selected on the command line via "--builders"
//! Sample call:
//! backtest-build-block --config /home/happy_programmer/config.toml --builders mgp-ordering --builders mp-ordering 19380913 --show-orders --show-missing

use ahash::HashMap;
use alloy_primitives::utils::format_ether;
use reth_db::Database;
use reth_provider::{BlockReader, DatabaseProviderFactory, StateProviderFactory};

use crate::{
    backtest::{
        execute::{backtest_prepare_ctx_for_block_from_building_context, BacktestBlockInput},
        OrdersWithTimestamp,
    },
    building::{builders::BacktestSimulateBlockInput, BlockBuildingContext},
    live_builder::cli::LiveBuilderConfig,
    primitives::{Order, OrderId, SimulatedOrder},
};
use clap::Parser;
use std::path::PathBuf;

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
}

/// Provides all the orders needed to simulate the construction of a block.
/// It also provides the needed context to execute those orders.
pub trait OrdersSource<ConfigType, DBType, ProviderType>
where
    ConfigType: LiveBuilderConfig,
    DBType: Database + Clone + 'static,
    ProviderType: DatabaseProviderFactory<DB = DBType, Provider: BlockReader>
        + StateProviderFactory
        + Clone
        + 'static,
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

pub async fn run_backtest_build_block<ConfigType, OrdersSourceType, DBType, ProviderType>(
    build_block_cfg: BuildBlockCfg,
    orders_source: OrdersSourceType,
) -> eyre::Result<()>
where
    ConfigType: LiveBuilderConfig,
    DBType: Database + Clone + 'static,
    ProviderType: DatabaseProviderFactory<DB = DBType, Provider: BlockReader>
        + StateProviderFactory
        + Clone
        + 'static,
    OrdersSourceType: OrdersSource<ConfigType, DBType, ProviderType>,
{
    let config = orders_source.config();
    config.base_config().setup_tracing_subscriber()?;

    let available_orders = orders_source.available_orders();
    println!("Available orders: {}", available_orders.len());

    if build_block_cfg.show_orders {
        print_order_and_timestamp(&available_orders, orders_source.block_time_as_unix_ms());
    }

    let provider_factory = orders_source.create_provider_factory()?;

    orders_source.print_custom_stats(provider_factory.clone())?;

    let ctx = orders_source.create_block_building_context()?;
    let BacktestBlockInput {
        ctx, sim_orders, ..
    } = backtest_prepare_ctx_for_block_from_building_context(
        ctx,
        available_orders.clone(),
        provider_factory.clone(),
        &config.base_config().sbundle_mergeable_signers(),
    )?;

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
                    cached_reads: None,
                };
                let build_res = config.build_backtest_block(builder_name, input);
                if let Err(err) = &build_res {
                    println!("Error building block: {:?}", err);
                    return None;
                }
                let (block, _) = build_res.ok()?;
                println!(
                    "Built block {} with builder: {:?}",
                    ctx.block(),
                    builder_name
                );
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
fn print_order_and_timestamp(orders_with_ts: &[OrdersWithTimestamp], block_time_as_unix_ms: u64) {
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
}

/// Print information about simulated orders.
fn print_simulated_orders(
    sim_orders: &[SimulatedOrder],
    order_and_timestamp: &HashMap<OrderId, u64>,
    block_time_as_unix_ms: u64,
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

        let slot_time_ms = timestamp_ms_to_slot_time(order_timestamp, block_time_as_unix_ms);

        println!(
            "{:>74} slot_time_ms: {:>8}, gas: {:>8} profit: {}",
            order.order.id().to_string(),
            slot_time_ms,
            order.sim_value.gas_used,
            format_ether(order.sim_value.coinbase_profit),
        );
    }
    println!();
}
