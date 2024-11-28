//! Backtest app to build a single block in a similar way as we do in live.
//! It gets the orders from a HistoricalDataStorage, simulates the orders and then runs the building algorithms.
//! It outputs the best algorithm (most profit) so we can check for improvements in our [crate::building::builders::BlockBuildingAlgorithm]s
//! BlockBuildingAlgorithm are defined on the config file but selected on the command line via "--builders"
//! Sample call:
//! backtest-build-block --config /home/happy_programmer/config.toml --builders mgp-ordering --builders mp-ordering 19380913 --show-orders --show-missing

use ahash::HashMap;
use alloy_primitives::utils::format_ether;

use crate::{
    backtest::{
        execute::{backtest_prepare_ctx_for_block, BacktestBlockInput},
        restore_landed_orders::{
            restore_landed_orders, sim_historical_block, ExecutedBlockTx, ExecutedTxs,
            SimplifiedOrder,
        },
        BlockData, HistoricalDataStorage, OrdersWithTimestamp,
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
    #[clap(long, help = "Show landed block txs values")]
    sim_landed_block: bool,
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

pub async fn run_backtest_build_block<ConfigType>() -> eyre::Result<()>
where
    ConfigType: LiveBuilderConfig,
{
    let cli = Cli::parse();

    let config: ConfigType = load_config_toml_and_env(cli.config)?;
    config.base_config().setup_tracing_subscriber()?;

    let block_data = read_block_data(
        &config.base_config().backtest_fetch_output_file,
        cli.block,
        cli.only_order_ids,
        cli.block_building_time_ms,
        cli.show_missing,
    )
    .await?;

    let (orders, order_and_timestamp): (Vec<Order>, HashMap<OrderId, u64>) = block_data
        .available_orders
        .iter()
        .map(|order| (order.order.clone(), (order.order.id(), order.timestamp_ms)))
        .unzip();

    println!("Available orders: {}", orders.len());

    if cli.show_orders {
        print_order_and_timestamp(&block_data.available_orders, &block_data);
    }

    let provider_factory = config.base_config().create_provider_factory()?;
    let chain_spec = config.base_config().chain_spec()?;
    let sbundle_mergeabe_signers = config.base_config().sbundle_mergeabe_signers();

    if cli.sim_landed_block {
        let tx_sim_results = sim_historical_block(
            provider_factory.clone(),
            chain_spec.clone(),
            block_data.onchain_block.clone(),
        )?;
        print_onchain_block_data(tx_sim_results, &orders, &block_data);
    }

    let BacktestBlockInput {
        ctx, sim_orders, ..
    } = backtest_prepare_ctx_for_block(
        block_data.clone(),
        provider_factory.clone(),
        chain_spec.clone(),
        cli.block_building_time_ms,
        config.base_config().blocklist()?,
        config.base_config().coinbase_signer()?,
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
                    provider: provider_factory.clone(),
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

/// Reads from HistoricalDataStorage the BlockData for block.
/// only_order_ids: if not empty returns only the given order ids.
/// block_building_time_ms: If not 0, time it took to build the block. It allows us to filter out orders that arrived after we started building the block (filter_late_orders).
/// show_missing: show on-chain orders that weren't available to us at building time.
async fn read_block_data(
    backtest_fetch_output_file: &PathBuf,
    block: u64,
    only_order_ids: Vec<String>,
    block_building_time_ms: i64,
    show_missing: bool,
) -> eyre::Result<BlockData> {
    let mut historical_data_storage =
        HistoricalDataStorage::new_from_path(backtest_fetch_output_file).await?;

    let mut block_data = historical_data_storage.read_block_data(block).await?;

    if !only_order_ids.is_empty() {
        block_data.filter_orders_by_ids(&only_order_ids);
    }
    if block_building_time_ms != 0 {
        block_data.filter_late_orders(block_building_time_ms);
    }

    if show_missing {
        show_missing_txs(&block_data);
    }

    println!(
        "Block: {} {:?}",
        block_data.block_number, block_data.onchain_block.header.hash
    );
    println!(
        "bid value: {}",
        format_ether(block_data.winning_bid_trace.value)
    );
    println!(
        "builder pubkey: {:?}",
        block_data.winning_bid_trace.builder_pubkey
    );
    Ok(block_data)
}

/// Convert a timestamp in milliseconds to the slot time relative to the given block timestamp.
fn timestamp_ms_to_slot_time(timestamp_ms: u64, block_timestamp: u64) -> i64 {
    (block_timestamp * 1000) as i64 - (timestamp_ms as i64)
}

/// Print the available orders sorted by timestamp.
fn print_order_and_timestamp(orders_with_ts: &[OrdersWithTimestamp], block_data: &BlockData) {
    let mut order_by_ts = orders_with_ts.to_vec();
    order_by_ts.sort_by_key(|owt| owt.timestamp_ms);
    for owt in order_by_ts {
        let id = owt.order.id();
        println!(
            "{:>74} ts: {}",
            id.to_string(),
            timestamp_ms_to_slot_time(
                owt.timestamp_ms,
                timestamp_as_u64(&block_data.onchain_block)
            )
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

fn print_onchain_block_data(
    tx_sim_results: Vec<ExecutedTxs>,
    orders: &[Order],
    block_data: &BlockData,
) {
    let mut executed_orders = Vec::new();

    let txs_to_idx: HashMap<_, _> = tx_sim_results
        .iter()
        .enumerate()
        .map(|(idx, tx)| (tx.hash(), idx))
        .collect();

    println!("Onchain block txs:");
    for (idx, tx) in tx_sim_results.into_iter().enumerate() {
        println!(
            "{:>4}, {:>74} revert: {:>5} profit: {}",
            idx,
            tx.hash(),
            !tx.receipt.success,
            format_ether(tx.coinbase_profit)
        );
        if !tx.conflicting_txs.is_empty() {
            println!("   conflicts: ");
        }
        for (tx, slots) in &tx.conflicting_txs {
            for slot in slots {
                println!(
                    "   {:>4} address: {:?>24}, key: {:?}",
                    txs_to_idx.get(tx).unwrap(),
                    slot.address,
                    slot.key
                );
            }
        }
        executed_orders.push(ExecutedBlockTx::new(
            tx.hash(),
            tx.coinbase_profit,
            tx.receipt.success,
        ))
    }

    // restored orders
    let mut simplified_orders = Vec::new();
    for order in orders {
        if block_data
            .built_block_data
            .as_ref()
            .map(|bd| bd.included_orders.contains(&order.id()))
            .unwrap_or(true)
        {
            simplified_orders.push(SimplifiedOrder::new_from_order(order));
        }
    }
    let restored_orders = restore_landed_orders(executed_orders, simplified_orders);

    for (id, order) in &restored_orders {
        println!(
            "{:>74} total_profit: {}, unique_profit: {}, error: {:?}",
            id,
            format_ether(order.total_coinbase_profit),
            format_ether(order.unique_coinbase_profit),
            order.error
        );
    }

    if let Some(built_block) = &block_data.built_block_data {
        println!();
        println!("Included orders:");
        for included_order in &built_block.included_orders {
            if let Some(order) = restored_orders.get(included_order) {
                println!(
                    "{:>74} total_profit: {}, unique_profit: {}, error: {:?}",
                    order.order,
                    format_ether(order.total_coinbase_profit),
                    format_ether(order.unique_coinbase_profit),
                    order.error
                );
                for (other, tx) in &order.overlapping_txs {
                    println!("    overlap with: {:>74} tx {:?}", other, tx);
                }
            } else {
                println!("{:>74} included order not found: ", included_order);
            }
        }
    }
}
