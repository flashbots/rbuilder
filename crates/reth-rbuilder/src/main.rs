//! `rbuilder` running in-process with vanilla reth.
//!
//! Usage: `cargo run -r --bin reth-rbuilder -- node --rbuilder.config <path-to-your-config-toml>`
//!
//! Note this method of running rbuilder is not quite ready for production.
//! See <https://github.com/flashbots/rbuilder/issues/229> for more information.

use clap::{Args, Parser};
use rbuilder::{
    live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig, config::Config},
    provider::reth_prov::StateProviderFactoryFromRethProvider,
    telemetry,
};
use reth::{chainspec::EthereumChainSpecParser, cli::Cli, primitives::Header};
use reth_node_builder::{engine_tree_config::TreeConfig, EngineNodeLauncher};
use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
use reth_provider::{
    providers::{BlockchainProvider, BlockchainProvider2},
    BlockReader, DatabaseProviderFactory, HeaderProvider, StateCommitmentProvider,
};
use reth_transaction_pool::{blobstore::DiskFileBlobStore, EthTransactionPool};
use std::{path::PathBuf, process};
use tokio::task;
use tracing::{error, info, warn};

// Prefer jemalloc for performance reasons.
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Debug, Clone, Args, PartialEq, Eq, Default)]
pub struct ExtraArgs {
    /// Path of the rbuilder config to use
    #[arg(long = "rbuilder.config")]
    pub rbuilder_config: PathBuf,

    /// Enable the experimental engine features on reth binary
    ///
    /// DEPRECATED: experimental engine is default now, use --engine.legacy to enable the legacy
    /// functionality
    #[arg(long = "engine.experimental", default_value = "false")]
    pub experimental: bool,

    /// Enable the legacy engine on reth binary
    #[arg(long = "engine.legacy", default_value = "false")]
    pub legacy: bool,

    /// Configure persistence threshold for engine experimental.
    #[arg(
        long = "engine.persistence-threshold",
        conflicts_with = "legacy",
        default_value_t = 0
    )]
    pub persistence_threshold: u64,

    /// Configure the target number of blocks to keep in memory.
    #[arg(
        long = "engine.memory-block-buffer-target",
        conflicts_with = "legacy",
        default_value_t = 0
    )]
    pub memory_block_buffer_target: u64,
}

fn main() {
    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    if let Err(err) =
        Cli::<EthereumChainSpecParser, ExtraArgs>::parse().run(|builder, extra_args| async move {
            if extra_args.experimental {
                warn!(target: "reth::cli", "Experimental engine is default now, and the --engine.experimental flag is deprecated. To enable the legacy functionality, use --engine.legacy.");
            }

            let use_legacy_engine = extra_args.legacy;
            match use_legacy_engine {
                false => {
                    let engine_tree_config = TreeConfig::default()
                        .with_persistence_threshold(extra_args.persistence_threshold)
                        .with_memory_block_buffer_target(extra_args.memory_block_buffer_target);
                    let handle = builder
                        .with_types_and_provider::<EthereumNode, BlockchainProvider2<_>>()
                        .with_components(EthereumNode::components())
                        .with_add_ons(EthereumAddOns::default())
                        .on_node_started(move |node| {
                            spawn_rbuilder(node.provider().clone(), node.pool().clone(), extra_args.rbuilder_config);
                            Ok(())
                        })
                        .launch_with_fn(|builder| {
                            let launcher = EngineNodeLauncher::new(
                                builder.task_executor().clone(),
                                builder.config().datadir(),
                                engine_tree_config,
                            );
                            builder.launch_with(launcher)
                        })
                        .await?;
                    handle.node_exit_future.await
                }
                true => {
                    info!(target: "reth::cli", "Running with legacy engine");
                    let handle = builder
                        .with_types_and_provider::<EthereumNode, BlockchainProvider<_>>()
                        .with_components(EthereumNode::components())
                        .with_add_ons::<EthereumAddOns<_>>(Default::default())
                        .on_node_started(move |node| {
                            spawn_rbuilder(node.provider().clone(), node.pool().clone(), extra_args.rbuilder_config);
                            Ok(())
                        })
                        .launch().await?;
                    handle.node_exit_future.await
                }
            }
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

/// Spawns a tokio rbuilder task.
///
/// Takes down the entire process if the rbuilder errors or stops.
fn spawn_rbuilder<P>(
    provider: P,
    pool: EthTransactionPool<P, DiskFileBlobStore>,
    config_path: PathBuf,
) where
    P: DatabaseProviderFactory<Provider: BlockReader>
        + reth_provider::StateProviderFactory
        + HeaderProvider<Header = Header>
        + StateCommitmentProvider
        + Clone
        + 'static,
{
    let _handle = task::spawn(async move {
        let result = async {
            let config: Config = load_config_toml_and_env(config_path)?;

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

            let builder = config
                .new_builder(
                    StateProviderFactoryFromRethProvider::new(
                        provider,
                        config.base_config().live_root_hash_config()?,
                    ),
                    Default::default(),
                )
                .await?;
            builder.connect_to_transaction_pool(pool).await?;
            builder.run().await?;

            Ok::<(), eyre::Error>(())
        }
        .await;

        if let Err(e) = result {
            error!("Fatal rbuilder error: {:#}", e);
            process::exit(1);
        }

        error!("rbuilder stopped unexpectedly");
        process::exit(1);
    });
}
