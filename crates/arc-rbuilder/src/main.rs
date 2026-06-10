//! `rbuilder` building blocks for the Arc chain, running in-process with an
//! arc-node execution node.
//!
//! Usage: `cargo run -p arc-rbuilder -- node --chain arc-testnet --rbuilder.config <config.toml>`
//!
//! Malachite consensus drives block production through the engine API; this
//! binary wraps the regular Arc payload builder so that `engine_getPayload`
//! returns rbuilder's best block, with the stock Arc builder as fallback.

use clap::{Args, Parser};
use rbuilder::{
    live_builder::{
        base_config::BaseConfig,
        block_output::engine_payload_sink::{EnginePayloadRegistry, EnginePayloadSinkFactory},
        cli::{create_start_slot_watchdog, LiveBuilderConfig as _},
        config::{create_builders, Config},
        payload_events::{MevBoostSlotData, SlotSource},
    },
    provider::reth_prov::StateProviderFactoryFromRethProvider,
    telemetry,
};
use rbuilder_config::load_toml_config;
use reth::{chainspec::EthereumHardforks, cli::Cli, primitives::Header};
use reth_provider::{
    providers::BlockchainProvider, BlockReader, ChainSpecProvider, ChangeSetReader,
    DatabaseProviderFactory, HeaderProvider, PruneCheckpointReader, StageCheckpointReader,
    StorageChangeSetReader, StorageSettingsCache,
};
use std::{path::PathBuf, process, sync::atomic::AtomicBool, sync::Arc};
use tokio::{sync::mpsc, task};
use tokio_util::sync::CancellationToken;
use tracing::error;

use arc_evm::{ArcEvmConfig, ArcEvmFactory};
use arc_evm_node::node::{ArcAddOns, ArcNode};
use arc_evm_node::ArcRpcLayer;
use arc_execution_config::chainspec::{ArcChainSpec, ArcChainSpecParser};
use arc_execution_payload::payload::ArcNetworkPayloadBuilderBuilder;
mod payload_service;
use payload_service::{RbuilderBridge, RbuilderPayloadServiceBuilder};

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

/// Components used by reth CLI commands (init, import, ...) that need an EVM
/// + consensus instance. Mirrors arc-node's `arc_components`.
fn arc_components(
    spec: Arc<ArcChainSpec>,
) -> (
    ArcEvmConfig,
    Arc<arc_execution_validation::ArcConsensus<ArcChainSpec>>,
) {
    let eth_evm = reth_node_ethereum::EthEvmConfig::new_with_evm_factory(
        spec.clone(),
        ArcEvmFactory::new(spec.clone()),
    );
    (
        ArcEvmConfig::new(eth_evm),
        Arc::new(arc_execution_validation::ArcConsensus::new(spec)),
    )
}

fn main() {
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    if let Err(err) = Cli::<ArcChainSpecParser, ExtraArgs>::parse().run_with_components::<ArcNode>(
        arc_components,
        async move |builder, extra_args| {
            let (slot_sender, slot_receiver) = mpsc::unbounded_channel();
            let bridge = RbuilderBridge::new(slot_sender);
            let registry = bridge.registry.clone();

            let arc_node = ArcNode::default();

            // Same components as a stock arc node, with the payload service
            // wrapped so rbuilder gets first shot at every payload job.
            let fallback_payload_builder = ArcNetworkPayloadBuilderBuilder::new(
                None,
                arc_node.payload_builder_deadline_ms,
                arc_node.wait_for_payload,
            );
            let components = ArcNode::components(
                &arc_node.invalid_tx_list_cfg,
                &arc_node.addresses_denylist_config,
                arc_node.payload_builder_deadline_ms,
                arc_node.wait_for_payload,
                arc_node.rebroadcast_interval,
            )
            .payload(RbuilderPayloadServiceBuilder::new(
                fallback_payload_builder,
                bridge,
            ));

            let add_ons = ArcAddOns::default()
                .with_arc_rpc_config(arc_node.rpc_cfg.clone())
                .with_rpc_middleware(ArcRpcLayer::new(
                    arc_node.filter_pending_txs,
                    arc_node.max_response_body_size as usize,
                ));

            let handle = builder
                .with_types_and_provider::<ArcNode, BlockchainProvider<_>>()
                .with_components(components)
                .with_add_ons(add_ons)
                .on_node_started(move |node| {
                    spawn_rbuilder(
                        node.provider().clone(),
                        node.pool().clone(),
                        extra_args.rbuilder_config,
                        registry,
                        slot_receiver,
                    );
                    Ok(())
                })
                .launch()
                .await?;
            handle.node_exit_future.await
        },
    ) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

/// Spawns a tokio rbuilder task wired to the engine payload bridge.
///
/// Takes down the entire process if the rbuilder errors or stops.
fn spawn_rbuilder<P, V, T, S>(
    provider: P,
    pool: reth_transaction_pool::Pool<V, T, S>,
    config_path: PathBuf,
    registry: Arc<EnginePayloadRegistry>,
    slot_receiver: mpsc::UnboundedReceiver<MevBoostSlotData>,
) where
    P: DatabaseProviderFactory<
            Provider: BlockReader
                          + StageCheckpointReader
                          + PruneCheckpointReader
                          + ChangeSetReader
                          + StorageChangeSetReader
                          + StorageSettingsCache,
        > + reth_provider::StateProviderFactory
        + HeaderProvider<Header = Header>
        + reth_provider::ChainSpecProvider
        + Clone
        + 'static,
    <P as ChainSpecProvider>::ChainSpec: EthereumHardforks,
    V: reth_transaction_pool::TransactionValidator<
            Transaction = reth_transaction_pool::EthPooledTransaction,
        > + 'static,
    T: reth_transaction_pool::TransactionOrdering<
        Transaction = <V as reth_transaction_pool::TransactionValidator>::Transaction,
    >,
    S: reth_transaction_pool::blobstore::BlobStore,
{
    let _handle = task::spawn(async move {
        let result = run_rbuilder(provider, pool, config_path, registry, slot_receiver).await;

        if let Err(e) = result {
            error!("Fatal rbuilder error: {:#}", e);
            process::exit(1);
        }

        error!("rbuilder stopped unexpectedly");
        process::exit(1);
    });
}

async fn run_rbuilder<P, V, T, S>(
    provider: P,
    pool: reth_transaction_pool::Pool<V, T, S>,
    config_path: PathBuf,
    registry: Arc<EnginePayloadRegistry>,
    slot_receiver: mpsc::UnboundedReceiver<MevBoostSlotData>,
) -> eyre::Result<()>
where
    P: DatabaseProviderFactory<
            Provider: BlockReader
                          + StageCheckpointReader
                          + PruneCheckpointReader
                          + ChangeSetReader
                          + StorageChangeSetReader
                          + StorageSettingsCache,
        > + reth_provider::StateProviderFactory
        + HeaderProvider<Header = Header>
        + reth_provider::ChainSpecProvider
        + Clone
        + 'static,
    <P as ChainSpecProvider>::ChainSpec: EthereumHardforks,
    V: reth_transaction_pool::TransactionValidator<
            Transaction = reth_transaction_pool::EthPooledTransaction,
        > + 'static,
    T: reth_transaction_pool::TransactionOrdering<
        Transaction = <V as reth_transaction_pool::TransactionValidator>::Transaction,
    >,
    S: reth_transaction_pool::blobstore::BlobStore,
{
    let config: Config = load_toml_config(config_path)?;
    let base_config: &BaseConfig = config.base_config();
    let cancel = CancellationToken::new();
    let abort = CancellationToken::new();
    let start_slot_watchdog_sender = create_start_slot_watchdog(base_config, cancel.clone())?;

    let ready_to_build = Arc::new(AtomicBool::new(false));
    telemetry::servers::redacted::spawn(
        base_config.redacted_telemetry_server_address(),
        ready_to_build.clone(),
    )
    .await?;
    telemetry::servers::full::spawn(
        base_config.full_telemetry_server_address(),
        config.version_for_telemetry(),
    )
    .await?;

    let blocklist_provider = base_config.blocklist_provider(cancel.clone()).await?;

    let state_provider_factory = StateProviderFactoryFromRethProvider::new(
        provider,
        base_config.live_root_hash_config()?,
    );

    let sink_factory = EnginePayloadSinkFactory::new(registry);

    let live_builder = base_config
        .create_builder_with_provider_factory(
            cancel.clone(),
            abort,
            Box::new(sink_factory),
            SlotSource::Channel(slot_receiver),
            state_provider_factory,
            blocklist_provider,
        )
        .await?;

    let builders = create_builders(
        config.live_builders()?,
        base_config.max_order_execution_duration_warning(),
    );
    let live_builder = live_builder.with_builders(builders);

    live_builder.connect_to_transaction_pool(pool).await?;
    live_builder
        .run(ready_to_build, start_slot_watchdog_sender)
        .await?;

    Ok(())
}
