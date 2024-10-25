//! `rbuilder` running in-process with vanilla reth.
//!
//! Usage: `cargo run -r --bin reth-rbuilder -- node --rbuilder.config <path-to-your-config-toml>`
//!
//! Note this method of running rbuilder is not quite ready for production.
//! See <https://github.com/flashbots/rbuilder/issues/229> for more information.

use clap::Args;
use rbuilder::{
    live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig, config::Config},
    telemetry,
};
use reth_db_api::Database;
use reth_provider::{
    providers::BlockchainProvider, DatabaseProviderFactory, HeaderProvider, StateProviderFactory,
};
use std::{path::PathBuf, process};
use tokio::task;
use tracing::error;

// Prefer jemalloc for performance reasons.
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Debug, Clone, Args, PartialEq, Eq, Default)]
pub struct ExtraArgs {
    /// Path of the rbuilder config to use
    #[arg(long = "rbuilder.config")]
    pub rbuilder_config: PathBuf,
    /// Enable the engine2 experimental features on reth binary
    #[arg(long = "engine.experimental", default_value = "false")]
    pub experimental: bool,
}

fn main() {
    use clap::Parser;
    use reth::cli::Cli;
    use reth_node_builder::EngineNodeLauncher;
    use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
    use reth_provider::providers::BlockchainProvider2;

    reth_cli_util::sigsegv_handler::install();

    if let Err(err) = Cli::<ExtraArgs>::parse().run(|builder, extra_args| async move {
        let enable_engine2 = extra_args.experimental;
        match enable_engine2 {
            true => {
                let handle = builder
                    .with_types_and_provider::<EthereumNode, BlockchainProvider2<_>>()
                    .with_components(EthereumNode::components())
                    .with_add_ons::<EthereumAddOns>()
                    .on_rpc_started(move |ctx, _| {
                        spawn_rbuilder(ctx.provider().clone(), extra_args.rbuilder_config);
                        Ok(())
                    })
                    .launch_with_fn(|builder| {
                        let launcher = EngineNodeLauncher::new(
                            builder.task_executor().clone(),
                            builder.config().datadir(),
                        );
                        builder.launch_with(launcher)
                    })
                    .await?;
                handle.node_exit_future.await
            }
            false => {
                let handle = builder
                    .with_types_and_provider::<EthereumNode, BlockchainProvider<_>>()
                    .with_components(EthereumNode::components())
                    .with_add_ons::<EthereumAddOns>()
                    .on_rpc_started(move |ctx, _| {
                        spawn_rbuilder(ctx.provider().clone(), extra_args.rbuilder_config);
                        Ok(())
                    })
                    .launch()
                    .await?;
                handle.node_exit_future.await
            }
        }
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

/// Spawns a tokio rbuilder task.
///
/// Takes down the entire process if the rbuilder errors or stops.
fn spawn_rbuilder<P, DB>(provider: P, config_path: PathBuf)
where
    DB: Database + Clone + 'static,
    P: DatabaseProviderFactory<DB> + StateProviderFactory + HeaderProvider + Clone + 'static,
{
    let _handle = task::spawn(async move {
        let result = async {
            let config: Config = load_config_toml_and_env(config_path)?;

            // TODO: Check removing this is OK. It seems reth already sets up the global tracing
            // subscriber, so this fails
            // config.base_config.setup_tracing_subscriber().expect("Failed to set up rbuilder tracing subscriber");

            // Spawn redacted server that is safe for tdx builders to expose
            telemetry::servers::redacted::spawn(
                config.base_config().redacted_telemetry_server_address(),
            )
            .await?;

            // Spawn debug server that exposes detailed operational information
            telemetry::servers::full::spawn(
                config.base_config.full_telemetry_server_address(),
                config.version_for_telemetry(),
                config.base_config.log_enable_dynamic,
            )
            .await?;
            let builder = config.new_builder(provider, Default::default()).await?;

            builder.run().await?;

            Ok::<(), eyre::Error>(())
        }
        .await;

        if let Err(e) = result {
            error!("Fatal rbuilder error: {}", e);
            process::exit(1);
        }

        error!("rbuilder stopped unexpectedly");
        process::exit(1);
    });
}
