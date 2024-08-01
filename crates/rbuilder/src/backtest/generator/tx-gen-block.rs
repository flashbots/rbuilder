use alloy_primitives::{Address, utils::format_ether};
use clap::Parser;
use rbuilder::{
    backtest::{
        execute::{backtest_prepare_ctx_for_block, BacktestBlockInput},
        generator::transaction_generator::{
            Precompile, Size, TransactionGenerator, TransactionType,
        },
        BlockData, HistoricalDataStorage,
    },
    building::{
        block_orders_from_sim_orders,
        builders::{
            merging_builder::merging_build_backtest, ordering_builder::OrderingBuilderContext,
        },
        sim::simulate_all_orders_with_sim_tree,
        BlockBuildingContext,
    },
    live_builder::config::{Config, SpecificBuilderConfig},
    primitives::{MempoolTx, Order, OrderId, SimulatedOrder},
};
use reth::tasks::pool::BlockingTaskPool;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
struct Cli {
    #[clap(long, help = "Config file path", env = "RBUILDER_CONFIG")]
    config: PathBuf,
    #[clap(help = "Block Number")]
    block: u64,
    #[clap(
        long,
        short,
        help = "Use suggested fee recipient as coinbase (only applies to OrderingBuilder)",
        default_value = "false"
    )]
    use_suggested_fee_recipient_as_coinbase: bool,
    #[clap(
        long,
        help = "builders to build block with (see config builders)",
        default_value = "mp-ordering"
    )]
    builders: Vec<String>,
}

const MAX_PAYMENT: u128 = 100_000_000_000_000_000; // 0.1 ETH

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    let config = Config::load_config_toml_and_env(cli.config)?;
    config.setup_tracing_subsriber()?;

    let mut historical_data_storage =
        HistoricalDataStorage::new_from_path(&config.backtest_fetch_output_file).await?;

    let mut block_data = historical_data_storage.read_block_data(cli.block).await?;

    let mut generator = TransactionGenerator::new(100);

    let available_orders = generator
        .set_transaction_type(TransactionType::Eip1559)
        .generate_conflicting_transactions(10, 250, 5 as f64);

    let addresses = generator.get_signer_addresses();
    let balances_to_increase = addresses
        .iter()
        .map(|address| (address.clone(), MAX_PAYMENT*10))
        .collect::<Vec<(Address, u128)>>();

    println!("Available orders: {:?}", available_orders.len());

    let provider_factory = config.provider_factory()?.provider_factory_unchecked();
    let chain_spec = config.chain_spec()?;
    let sbundle_mergeabe_signers = config.sbundle_mergeabe_signers();

    let mut ctx = BlockBuildingContext::from_block_data(
        &block_data,
        chain_spec.clone(),
        config.blocklist()?,
        None,
    );

    ctx.set_balances_to_spoof(balances_to_increase);

    let (sim_orders, _) = simulate_all_orders_with_sim_tree(
        provider_factory.clone(),
        &ctx,
        &available_orders,
        false,
    )?;

    println!("Simulated orders: {:?}", sim_orders.len());

    let state_provider = provider_factory.history_by_block_number(cli.block - 1)?;

    let winning_builder = cli
        .builders
        .iter()
        .filter_map(|builder_name| {
            let builder = config.builder(builder_name).ok()?;
            let block = match builder.builder {
                SpecificBuilderConfig::OrderingBuilder(config) => {
                    let block_orders = block_orders_from_sim_orders(
                        &sim_orders,
                        config.sorting,
                        &state_provider,
                        &sbundle_mergeabe_signers,
                    )
                    .ok()?;
                    let mut builder = OrderingBuilderContext::new(
                        provider_factory.clone(),
                        Arc::new(()),
                        BlockingTaskPool::build().ok()?,
                        builder_name.clone(),
                        ctx.clone(),
                        config.clone(),
                    )
                    .with_skip_root_hash();
                    builder
                        .build_block(block_orders, cli.use_suggested_fee_recipient_as_coinbase)
                        .ok()?
                        .ok_or_else(|| eyre::eyre!("No block built"))
                        .ok()?
                }
                SpecificBuilderConfig::MergingBuilder(config) => {
                    let (block, _) = merging_build_backtest(
                        provider_factory.clone(),
                        config,
                        builder_name.clone(),
                        sim_orders.clone(),
                        ctx.clone(),
                        None,
                    )
                    .ok()?;
                    block
                }
            };
            println!("Built block {} with builder: {:?}", cli.block, builder.name);
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

    Ok(())
}
