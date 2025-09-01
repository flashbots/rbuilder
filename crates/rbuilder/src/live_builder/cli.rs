use std::{
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
};

use clap::Parser;
use serde::de::DeserializeOwned;
use std::fmt::Debug;
use sysperf::{format_results, gather_system_info, run_all_benchmarks};
use tokio::signal::ctrl_c;
use tokio_util::sync::CancellationToken;

use crate::{
    building::{
        builders::{BacktestSimulateBlockInput, Block},
        PartialBlockExecutionTracer,
    },
    live_builder::{
        base_config::load_config_toml_and_env, payload_events::MevBoostSlotDataGenerator,
    },
    provider::StateProviderFactory,
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
    fn new_builder<P>(
        &self,
        provider: P,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = eyre::Result<LiveBuilder<P, MevBoostSlotDataGenerator>>> + Send
    where
        P: StateProviderFactory + Clone + 'static;

    /// Patch until we have a unified way of backtesting using the exact algorithms we use on the LiveBuilder.
    /// building_algorithm_name will come from the specific configuration.
    fn build_backtest_block<
        P,
        PartialBlockExecutionTracerType: PartialBlockExecutionTracer + Clone + Send + Sync + 'static,
    >(
        &self,
        building_algorithm_name: &str,
        input: BacktestSimulateBlockInput<'_, P>,
        partial_block_execution_tracer: PartialBlockExecutionTracerType,
    ) -> eyre::Result<Block>
    where
        P: StateProviderFactory + Clone + 'static;
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
            println!("{config:#?}");
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
            println!("0x{address}");
            return Ok(());
        }
    };

    let config: ConfigType = load_config_toml_and_env(cli.config)?;
    config.base_config().setup_tracing_subscriber()?;

    let ready_to_build = Arc::new(AtomicBool::new(false));
    // Spawn redacted server that is safe for tdx builders to expose
    telemetry::servers::redacted::spawn(
        config.base_config().redacted_telemetry_server_address(),
        ready_to_build.clone(),
    )
    .await?;

    // Spawn debug server that exposes detailed operational information
    telemetry::servers::full::spawn(
        config.base_config().full_telemetry_server_address(),
        config.version_for_telemetry(),
    )
    .await?;
    if config.base_config().ipc_provider.is_some() {
        let provider = config.base_config().create_ipc_provider_factory()?;
        run_builder(provider, config, on_run, ready_to_build).await
    } else {
        let provider = config.base_config().create_reth_provider_factory(false)?;
        run_builder(provider, config, on_run, ready_to_build).await
    }
}

async fn run_builder<P, ConfigType>(
    provider: P,
    config: ConfigType,
    on_run: Option<fn()>,
    ready_to_build: Arc<AtomicBool>,
) -> eyre::Result<()>
where
    ConfigType: LiveBuilderConfig,
    P: StateProviderFactory + Clone + 'static,
{
    let cancel = CancellationToken::new();
    let builder = config.new_builder(provider, cancel.clone()).await?;

    let ctrlc = tokio::spawn(async move {
        ctrl_c().await.unwrap_or_default();
        cancel.cancel()
    });
    if let Some(on_run) = on_run {
        on_run();
    }
    builder.run(ready_to_build).await?;
    ctrlc.await.unwrap_or_default();
    Ok(())
}
