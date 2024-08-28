use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use reth_db::DatabaseEnv;
use reth_payload_builder::database::CachedReads;
use tokio::signal::ctrl_c;
use tokio_util::sync::CancellationToken;

use crate::{
    building::builders::{BacktestSimulateBlockInput, Block},
    live_builder::{
        base_config::load_config_toml_and_env, payload_events::MevBoostSlotDataGenerator,
    },
    provider::StateProviderFactory,
    telemetry::spawn_telemetry_server,
    utils::{build_info::Version, ProviderFactoryReopener},
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
}

#[derive(Parser, Debug)]
struct RunCmd {
    #[clap(help = "Config file path")]
    config: PathBuf,
}

/// Basic stuff needed to call cli::run
pub trait LiveBuilderConfig: std::fmt::Debug + serde::de::DeserializeOwned {
    fn base_config(&self) -> &BaseConfig;
    /// Version reported by telemetry
    fn version_for_telemetry(&self) -> Version;
    /// Desugared from async to future to keep clippy happy
    fn create_builder(
        &self,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<
        Output = eyre::Result<
            LiveBuilder<ProviderFactoryReopener<Arc<DatabaseEnv>>, MevBoostSlotDataGenerator>,
        >,
    > + Send;

    /// Patch until we have a unified way of backtesting using the exact algorithms we use on the LiveBuilder.
    /// building_algorithm_name will come from the specific configuration.
    fn build_backtest_block<Provider: StateProviderFactory + Clone + 'static>(
        &self,
        building_algorithm_name: &str,
        input: BacktestSimulateBlockInput<'_, Provider>,
    ) -> eyre::Result<(Block, CachedReads)>;
}

/// print_version_info func that will be called on command Cli::Version
/// on_run func that will be called on command Cli::Run just before running
pub async fn run<ConfigType: LiveBuilderConfig>(
    print_version_info: fn(),
    on_run: Option<fn()>,
) -> eyre::Result<()> {
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
    };

    let config: ConfigType = load_config_toml_and_env(cli.config)?;
    config.base_config().setup_tracing_subsriber()?;

    let cancel = CancellationToken::new();

    spawn_telemetry_server(
        config.base_config().telemetry_address(),
        config.version_for_telemetry(),
    )
    .await?;
    let builder = config.create_builder(cancel.clone()).await?;

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
