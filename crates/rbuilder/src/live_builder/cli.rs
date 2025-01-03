use std::path::PathBuf;

use clap::Parser;
use reth::revm::cached::CachedReads;
use reth_db::Database;
use reth_provider::{BlockReader, DatabaseProviderFactory, HeaderProvider, StateProviderFactory};
use serde::de::DeserializeOwned;
use std::fmt::Debug;
use sysperf::{format_results, gather_system_info, run_all_benchmarks};
use tokio::signal::ctrl_c;
use tokio_util::sync::CancellationToken;

use crate::{
    building::builders::{BacktestSimulateBlockInput, Block},
    live_builder::{
        base_config::load_config_toml_and_env, payload_events::MevBoostSlotDataGenerator,
    },
    telemetry,
    utils::{bls::generate_random_bls_address, build_info::Version},
};

use super::{base_config::BaseConfig, LiveBuilder};

#[derive(Parser, Debug)]
enum Cli {
    #[clap(name = "run", about = "Run the builder")]
    Run(RunCmd),
    #[clap(name = "config", about = "Print the current config")]
    Config(RunCmd),
    #[clap(name = "version", about = "Print version information")]
    Version,
    #[clap(
        name = "sysperf",
        about = "Run system performance benchmarks (CPU, disk, memory)"
    )]
    SysPerf,
    #[clap(name = "gen-bls", about = "Generate a BLS signature")]
    GenBls,
}

#[derive(Parser, Debug)]
struct RunCmd {
    #[clap(env = "RBUILDER_CONFIG", help = "Config file path")]
    config: PathBuf,
}

/// Basic stuff needed to call cli::run
pub trait LiveBuilderConfig: Debug + DeserializeOwned + Sync {
    fn base_config(&self) -> &BaseConfig;
    /// Version reported by telemetry
    fn version_for_telemetry(&self) -> Version;

    /// Create a concrete builder
    ///
    /// Desugared from async to future to keep clippy happy
    fn new_builder<P, DB>(
        &self,
        provider: P,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = eyre::Result<LiveBuilder<P, DB, MevBoostSlotDataGenerator>>>
           + Send
    where
        DB: Database + Clone + 'static,
        P: DatabaseProviderFactory<DB = DB, Provider: BlockReader>
            + StateProviderFactory
            + HeaderProvider
            + Clone
            + 'static;

    /// Patch until we have a unified way of backtesting using the exact algorithms we use on the LiveBuilder.
    /// building_algorithm_name will come from the specific configuration.
    fn build_backtest_block<P, DB>(
        &self,
        building_algorithm_name: &str,
        input: BacktestSimulateBlockInput<'_, P>,
    ) -> eyre::Result<(Block, CachedReads)>
    where
        DB: Database + Clone + 'static,
        P: DatabaseProviderFactory<DB = DB, Provider: BlockReader>
            + StateProviderFactory
            + Clone
            + 'static;
}

/// print_version_info func that will be called on command Cli::Version
/// on_run func that will be called on command Cli::Run just before running
pub async fn run<ConfigType>(print_version_info: fn(), on_run: Option<fn()>) -> eyre::Result<()>
where
    ConfigType: LiveBuilderConfig,
{
    let cli = Cli::parse();
    let cli = match cli {
        Cli::Run(cli) => cli,
        Cli::Config(cli) => {
            let config: ConfigType = load_config_toml_and_env(cli.config)?;
            println!("{:#?}", config);
            return Ok(());
        }
        Cli::Version => {
            print_version_info();
            return Ok(());
        }
        Cli::SysPerf => {
            let result =
                run_all_benchmarks(&PathBuf::from("/tmp/benchmark_test.tmp"), 100, 100, 1000)?;

            let sysinfo = gather_system_info();
            println!("{}", format_results(&result, &sysinfo));
            return Ok(());
        }
        Cli::GenBls => {
            let address = generate_random_bls_address();
            println!("0x{}", address);
            return Ok(());
        }
    };

    let config: ConfigType = load_config_toml_and_env(cli.config)?;
    config.base_config().setup_tracing_subscriber()?;

    let cancel = CancellationToken::new();

    // Spawn redacted server that is safe for tdx builders to expose
    telemetry::servers::redacted::spawn(config.base_config().redacted_telemetry_server_address())
        .await?;

    // Spawn debug server that exposes detailed operational information
    telemetry::servers::full::spawn(
        config.base_config().full_telemetry_server_address(),
        config.version_for_telemetry(),
        config.base_config().log_enable_dynamic,
    )
    .await?;
    let provider = config.base_config().create_provider_factory()?;
    let builder = config.new_builder(provider, cancel.clone()).await?;

    let ctrlc = tokio::spawn(async move {
        ctrl_c().await.unwrap_or_default();
        cancel.cancel()
    });
    if let Some(on_run) = on_run {
        on_run();
    }
    builder.run().await?;

    ctrlc.await.unwrap_or_default();
    Ok(())
}
