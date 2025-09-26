//! `rbuilder` running in-process with vanilla reth.
//!
//! Usage: `cargo run -r --bin reth-rbuilder -- node --rbuilder.config <path-to-your-config-toml>`
//!
//! Note this method of running rbuilder is not quite ready for production.
//! See <https://github.com/flashbots/rbuilder/issues/229> for more information.

use clap::{Args, Parser};
use rbuilder::{
    live_builder::{cli::LiveBuilderConfig, config::Config},
    provider::reth_prov::StateProviderFactoryFromRethProvider,
    telemetry,
};
use rbuilder_config::load_toml_config;
use reth::{
    chainspec::{EthereumChainSpecParser, EthereumHardforks},
    cli::Cli,
    primitives::Header,
};
use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
use reth_provider::{
    providers::BlockchainProvider, BlockReader, ChainSpecProvider, DatabaseProviderFactory,
    HeaderProvider,
};
use reth_transaction_pool::{blobstore::DiskFileBlobStore, EthTransactionPool};
use std::{
    path::PathBuf,
    process,
    sync::{atomic::AtomicBool, Arc},
};
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
}

fn main() {
    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    if let Err(err) =
        Cli::<EthereumChainSpecParser, ExtraArgs>::parse().run(|builder, extra_args| async move {
            let handle = builder
                .with_types_and_provider::<EthereumNode, BlockchainProvider<_>>()
                .with_components(EthereumNode::components())
                .with_add_ons(EthereumAddOns::default())
                .on_node_started(move |node| {
                    spawn_rbuilder(
                        node.provider().clone(),
                        node.pool().clone(),
                        extra_args.rbuilder_config,
                    );
                    Ok(())
                })
                .launch()
                .await?;
            handle.node_exit_future.await
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
        + reth_provider::ChainSpecProvider
        + Clone
        + 'static,
    <P as ChainSpecProvider>::ChainSpec: EthereumHardforks,
{
    let _handle = task::spawn(async move {
        let result = async {
            let config: Config = load_toml_config(config_path)?;

            let ready_to_build = Arc::new(AtomicBool::new(false));
            // Spawn redacted server that is safe for tdx builders to expose
            telemetry::servers::redacted::spawn(
                config.base_config().redacted_telemetry_server_address(),
                ready_to_build.clone(),
            )
            .await?;

            // Spawn debug server that exposes detailed operational information
            telemetry::servers::full::spawn(
                config.base_config.full_telemetry_server_address(),
                config.version_for_telemetry(),
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
            builder.run(ready_to_build).await?;

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
