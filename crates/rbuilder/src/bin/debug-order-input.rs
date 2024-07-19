//! Application to test the orders input.
//! For each block it subscribes a ReplaceableOrderPrinter to the OrderPool.

use clap::Parser;
use ethers::{
    middleware::Middleware,
    providers::{Ipc, Provider},
};
use jsonrpsee::RpcModule;
use rbuilder::{
    live_builder::{
        base_config::load_config_toml_and_env,
        cli::LiveBuilderConfig,
        config::Config,
        order_input::{
            replaceable_order_sink::ReplaceableOrderPrinter, start_orderpool_jobs, OrderInputConfig,
        },
    },
    telemetry::spawn_telemetry_server,
    utils::build_info::rbuilder_version,
};
use std::path::PathBuf;
use tokio::signal::ctrl_c;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, log::debug};

#[derive(Parser, Debug)]
struct Cli {
    #[clap(long, help = "path to output csv file")]
    csv: Option<PathBuf>,
    #[clap(long, help = "maximum blocks to run", default_value = "1000000")]
    max_blocks: u64,
    #[clap(help = "Config file path")]
    config: PathBuf,
}

#[tokio::main]
pub async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    let config: Config = load_config_toml_and_env(cli.config)?;
    config.base_config().setup_tracing_subsriber()?;

    let cancel = CancellationToken::new();

    spawn_telemetry_server(config.base_config().telemetry_address(), rbuilder_version()).await?;

    let provider_factory = config.base_config().provider_factory()?;

    let order_input_config = OrderInputConfig::from_config(config.base_config());

    let (handle, order_pool_subscriber) = start_orderpool_jobs(
        order_input_config,
        provider_factory,
        RpcModule::new(()),
        cancel.clone(),
    )
    .await?;

    let ipc = Ipc::connect(config.base_config().el_node_ipc_path.clone()).await?;
    let provider = Provider::new(ipc);
    let mut block_sub = provider.subscribe_blocks().await?;
    let mut blocks_passed = 0;
    loop {
        let block_number = provider
            .get_block_number()
            .await
            .unwrap_or_default()
            .as_u64();
        info!("new block: {}", block_number);
        blocks_passed += 1;
        if blocks_passed > cli.max_blocks {
            break;
        }

        let _sub_id = order_pool_subscriber
            .add_sink_auto_remove(block_number + 1, Box::new(ReplaceableOrderPrinter {}));

        tokio::select! {
            _ = block_sub.next() => {
                debug!("new block event");
            },
            _ = ctrl_c() => {
                break;
            }
        }
    }

    info!("shutting down");
    cancel.cancel();
    handle.await.unwrap_or_default();
    Ok(())
}
