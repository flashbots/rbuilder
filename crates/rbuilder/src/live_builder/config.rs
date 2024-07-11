//! Config should always be deserializable, default values should be used
//!
use crate::{
    building::{
        builders::{
            ordering_builder::{OrderingBuilderConfig, OrderingBuildingAlgorithm},
            BacktestSimulateBlockInput, BestBlockCell, Block, BlockBuildingAlgorithm,
        },
        Sorting,
    },
    live_builder::cli::LiveBuilderConfig,
    utils::{build_info::rbuilder_version, ProviderFactoryReopener, Signer},
};
use alloy_primitives::{Address, B256};
use eyre::Context;
use reth::{
    primitives::{ChainSpec, StaticFileSegment},
    tasks::pool::BlockingTaskPool,
};
use reth_db::DatabaseEnv;
use reth_payload_builder::database::CachedReads;
use serde::Deserialize;
use serde_with::serde_as;
use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use super::base_config::BaseConfig;

/// This example has a single building algorithm cfg but the idea of this enum is to have several builders
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "algo", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SpecificBuilderConfig {
    OrderingBuilder(OrderingBuilderConfig),
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct BuilderConfig {
    pub name: String,
    #[serde(flatten)]
    pub builder: SpecificBuilderConfig,
}

#[serde_as]
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    #[serde(flatten)]
    pub base_config: BaseConfig,
    /// selected builder configurations
    pub builders: Vec<BuilderConfig>,
}

impl LiveBuilderConfig for Config {
    fn base_config(&self) -> &BaseConfig {
        &self.base_config
    }
    /// WARN: opens reth db
    async fn create_builder(
        &self,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> eyre::Result<
        super::LiveBuilder<Arc<DatabaseEnv>, super::building::relay_submit::RelaySubmitSinkFactory>,
    > {
        let live_builder = self.base_config.create_builder(cancellation_token).await?;
        let root_hash_task_pool = self.base_config.root_hash_task_pool()?;
        let builders = create_builders(
            self.live_builders()?,
            root_hash_task_pool,
            self.base_config.sbundle_mergeabe_signers(),
        );
        Ok(live_builder.with_builders(builders))
    }

    fn version_for_telemetry(&self) -> crate::utils::build_info::Version {
        rbuilder_version()
    }

    fn build_backtest_block(
        &self,
        building_algorithm_name: &str,
        input: BacktestSimulateBlockInput<'_, Arc<DatabaseEnv>>,
    ) -> eyre::Result<(Block, CachedReads)> {
        let builder_cfg = self.builder(building_algorithm_name)?;
        match builder_cfg.builder {
            SpecificBuilderConfig::OrderingBuilder(config) => {
                crate::building::builders::ordering_builder::backtest_simulate_block(config, input)
            }
        }
    }
}

impl Config {
    fn live_builders(&self) -> eyre::Result<Vec<BuilderConfig>> {
        self.base_config
            .live_builders
            .iter()
            .map(|cfg_name| self.builder(cfg_name))
            .collect()
    }

    fn builder(&self, name: &str) -> eyre::Result<BuilderConfig> {
        self.builders
            .iter()
            .find(|b| b.name == name)
            .cloned()
            .ok_or_else(|| eyre::eyre!("Builder {} not found in builders list", name))
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_config: Default::default(),
            builders: vec![
                BuilderConfig {
                    name: "mgp-ordering".to_string(),
                    builder: SpecificBuilderConfig::OrderingBuilder(OrderingBuilderConfig {
                        discard_txs: true,
                        sorting: Sorting::MevGasPrice,
                        failed_order_retries: 1,
                        drop_failed_orders: true,
                        coinbase_payment: false,
                        build_duration_deadline_ms: None,
                    }),
                },
                BuilderConfig {
                    name: "mp-ordering".to_string(),
                    builder: SpecificBuilderConfig::OrderingBuilder(OrderingBuilderConfig {
                        discard_txs: true,
                        sorting: Sorting::MaxProfit,
                        failed_order_retries: 1,
                        drop_failed_orders: true,
                        coinbase_payment: false,
                        build_duration_deadline_ms: None,
                    }),
                },
            ],
        }
    }
}

/// Open reth db and DB should be opened once per process but it can be cloned and moved to different threads.
pub fn create_provider_factory(
    reth_datadir: Option<&Path>,
    reth_db_path: Option<&Path>,
    reth_static_files_path: Option<&Path>,
    chain_spec: Arc<ChainSpec>,
) -> eyre::Result<ProviderFactoryReopener<Arc<DatabaseEnv>>> {
    let reth_db_path = match (reth_db_path, reth_datadir) {
        (Some(reth_db_path), _) => PathBuf::from(reth_db_path),
        (None, Some(reth_datadir)) => reth_datadir.join("db"),
        (None, None) => eyre::bail!("Either reth_db_path or reth_datadir must be provided"),
    };

    let db = open_reth_db(&reth_db_path)?;

    let reth_static_files_path = match (reth_static_files_path, reth_datadir) {
        (Some(reth_static_files_path), _) => PathBuf::from(reth_static_files_path),
        (None, Some(reth_datadir)) => reth_datadir.join("static_files"),
        (None, None) => {
            eyre::bail!("Either reth_static_files_path or reth_datadir must be provided")
        }
    };

    let provider_factory_reopener =
        ProviderFactoryReopener::new(db, chain_spec, reth_static_files_path)?;

    if provider_factory_reopener
        .provider_factory_unchecked()
        .static_file_provider()
        .get_highest_static_file_block(StaticFileSegment::Headers)
        .is_none()
    {
        eyre::bail!("No headers in static files. Check your static files path configuration.");
    }

    Ok(provider_factory_reopener)
}

fn open_reth_db(reth_db_path: &Path) -> eyre::Result<Arc<DatabaseEnv>> {
    Ok(Arc::new(
        reth_db::open_db_read_only(reth_db_path, Default::default()).context("DB open error")?,
    ))
}

pub fn coinbase_signer_from_secret_key(secret_key: &str) -> eyre::Result<Signer> {
    let secret_key = B256::from_str(secret_key)?;
    Ok(Signer::try_from_secret(secret_key)?)
}

fn create_builders(
    configs: Vec<BuilderConfig>,
    root_hash_task_pool: BlockingTaskPool,
    sbundle_mergeabe_signers: Vec<Address>,
) -> Vec<Arc<dyn BlockBuildingAlgorithm<Arc<DatabaseEnv>, BestBlockCell>>> {
    configs
        .into_iter()
        .map(|cfg| create_builder(cfg, &root_hash_task_pool, &sbundle_mergeabe_signers))
        .collect()
}

fn create_builder(
    cfg: BuilderConfig,
    root_hash_task_pool: &BlockingTaskPool,
    sbundle_mergeabe_signers: &[Address],
) -> Arc<dyn BlockBuildingAlgorithm<Arc<DatabaseEnv>, BestBlockCell>> {
    match cfg.builder {
        SpecificBuilderConfig::OrderingBuilder(order_cfg) => {
            Arc::new(OrderingBuildingAlgorithm::new(
                root_hash_task_pool.clone(),
                sbundle_mergeabe_signers.to_vec(),
                order_cfg,
                cfg.name,
            ))
        }
    }
}

#[cfg(test)]
mod test {
    use crate::live_builder::base_config::load_config_toml_and_env;

    use super::*;
    use alloy_primitives::address;
    use std::env;

    #[test]
    fn test_default_config() {
        let config: Config = serde_json::from_str("{}").unwrap();
        let config_default = Config::default();

        assert_eq!(config, config_default);
    }

    #[test]
    fn test_parse_example_config() {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("../../config-live-example.toml");

        let config: Config = load_config_toml_and_env(p.clone()).expect("Config load");

        assert_eq!(
            config
                .base_config
                .coinbase_signer()
                .expect_err("should be error")
                .to_string(),
            "Env variable: COINBASE_SECRET_KEY not set"
        );

        env::set_var(
            "COINBASE_SECRET_KEY",
            "0xb785cd753d62bb25c0afaf75fd40dd94bf295051fdadc972ec857ad6b29cfa72",
        );

        env::set_var("CL_NODE_URL", "http://localhost:3500");

        let config: Config = load_config_toml_and_env(p).expect("Config load");

        assert_eq!(
            config
                .base_config
                .coinbase_signer()
                .expect("Coinbase signer")
                .address,
            address!("75618c70B1BBF111F6660B0E3760387fb494102B")
        );

        assert!(config
            .base_config
            .cl_node_url
            .contains(&"http://localhost:3500".to_string()));
    }

    #[test]
    fn test_parse_backtest_example_config() {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("../../config-backtest-example.toml");

        load_config_toml_and_env::<Config>(p).expect("Config load");
    }
}
