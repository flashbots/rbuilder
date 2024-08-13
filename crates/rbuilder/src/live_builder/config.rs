//! Config should always be deserializable, default values should be used
//!
//!
use super::{base_config::BaseConfig, building::relay_submit::RelaySubmitSinkFactory};
use crate::{
    beacon_api_client::Client,
    building::{
        builders::{
            ordering_builder::{OrderingBuilderConfig, OrderingBuildingAlgorithm},
            BacktestSimulateBlockInput, BestBlockCell, Block, BlockBuildingAlgorithm,
        },
        Sorting,
    },
    flashbots::BlocksProcessorClient,
    live_builder::{
        base_config::EnvOrValue, building::SubmissionConfig, cli::LiveBuilderConfig,
        payload_events::MevBoostSlotDataGenerator,
    },
    mev_boost::BLSBlockSigner,
    primitives::mev_boost::{MevBoostRelay, RelayConfig},
    utils::{build_info::rbuilder_version, ProviderFactoryReopener, Signer},
    validation_api_client::ValidationAPIClient,
};
use alloy_chains::ChainKind;
use alloy_primitives::{
    utils::{format_ether, parse_ether},
    Address, FixedBytes, B256,
};
use ethereum_consensus::{
    builder::compute_builder_domain, crypto::SecretKey, primitives::Version,
    state_transition::Context as ContextEth,
};
use eyre::Context;
use reth::tasks::pool::BlockingTaskPool;
use reth_chainspec::{Chain, ChainSpec, NamedChain};
use reth_db::DatabaseEnv;
use reth_payload_builder::database::CachedReads;
use reth_primitives::StaticFileSegment;
use reth_provider::StaticFileProviderFactory;
use serde::Deserialize;
use serde_with::{serde_as, OneOrMany};
use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use tracing::info;
use url::Url;

/// From experience (Vitaly's) all generated blocks before slot_time-8sec end loosing (due to last moment orders?)
const DEFAULT_SLOT_DELTA_TO_START_SUBMITS: time::Duration = time::Duration::milliseconds(-8000);

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

    #[serde(flatten)]
    pub l1_config: L1Config,

    /// selected builder configurations
    pub builders: Vec<BuilderConfig>,
}

#[serde_as]
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct L1Config {
    // Relay Submission configuration
    pub relays: Vec<RelayConfig>,
    pub dry_run: bool,
    #[serde_as(deserialize_as = "OneOrMany<_>")]
    pub dry_run_validation_url: Vec<String>,
    /// Secret key that will be used to sign normal submissions to the relay.
    relay_secret_key: EnvOrValue<String>,
    /// Secret key that will be used to sign optimistic submissions to the relay.
    optimistic_relay_secret_key: EnvOrValue<String>,
    /// When enabled builer will make optimistic submissions to optimistic relays
    /// influenced by `optimistic_max_bid_value_eth` and `optimistic_prevalidate_optimistic_blocks`
    pub optimistic_enabled: bool,
    /// Bids above this value will always be submitted in non-optimistic mode.
    pub optimistic_max_bid_value_eth: String,
    /// If true all optimistic submissions will be validated on nodes specified in `dry_run_validation_url`
    pub optimistic_prevalidate_optimistic_blocks: bool,
    pub blocks_processor_url: Option<String>,

    // See [`SubmissionConfig`]
    slot_delta_to_start_submits_ms: Option<i64>,

    ///Name kept singular for backwards compatibility
    #[serde_as(deserialize_as = "OneOrMany<EnvOrValue<String>>")]
    pub cl_node_url: Vec<EnvOrValue<String>>,
}

impl Default for L1Config {
    fn default() -> Self {
        Self {
            relays: vec![],
            dry_run: false,
            dry_run_validation_url: vec![],
            relay_secret_key: "".into(),
            optimistic_relay_secret_key: "".into(),
            optimistic_enabled: false,
            optimistic_max_bid_value_eth: "0.0".to_string(),
            optimistic_prevalidate_optimistic_blocks: false,
            blocks_processor_url: None,
            slot_delta_to_start_submits_ms: None,
            cl_node_url: vec![EnvOrValue::from("http://127.0.0.1:3500")],
        }
    }
}

impl L1Config {
    pub fn resolve_cl_node_urls(&self) -> eyre::Result<Vec<String>> {
        crate::live_builder::base_config::resolve_env_or_values::<String>(&self.cl_node_url)
    }

    pub fn beacon_clients(&self) -> eyre::Result<Vec<Client>> {
        self.cl_node_url
            .iter()
            .map(|url| {
                let url = Url::parse(&url.value()?)?;
                Ok(Client::new(url))
            })
            .collect()
    }

    pub fn relays(&self) -> eyre::Result<Vec<MevBoostRelay>> {
        let mut results = Vec::new();
        for relay in &self.relays {
            results.push(MevBoostRelay::from_config(relay)?);
        }
        Ok(results)
    }

    pub fn bls_signer(&self, chain_spec: &ChainSpec) -> eyre::Result<BLSBlockSigner> {
        let signing_domain = get_signing_domain(chain_spec.chain, self.beacon_clients()?)?;
        let secret_key = self.relay_secret_key.value()?;
        let secret_key = SecretKey::try_from(secret_key)
            .map_err(|e| eyre::eyre!("Failed to parse relay key: {:?}", e.to_string()))?;

        BLSBlockSigner::new(secret_key, signing_domain)
    }

    pub fn bls_optimistic_signer(&self, chain_spec: &ChainSpec) -> eyre::Result<BLSBlockSigner> {
        let signing_domain = get_signing_domain(chain_spec.chain, self.beacon_clients()?)?;
        let secret_key = self.optimistic_relay_secret_key.value()?;
        let secret_key = SecretKey::try_from(secret_key).map_err(|e| {
            eyre::eyre!("Failed to parse optimistic relay key: {:?}", e.to_string())
        })?;

        BLSBlockSigner::new(secret_key, signing_domain)
    }

    pub fn slot_delta_to_start_submits(&self) -> time::Duration {
        self.slot_delta_to_start_submits_ms
            .map(time::Duration::milliseconds)
            .unwrap_or(DEFAULT_SLOT_DELTA_TO_START_SUBMITS)
    }

    pub fn submission_config(&self, chain_spec: Arc<ChainSpec>) -> eyre::Result<SubmissionConfig> {
        if (self.dry_run || self.optimistic_prevalidate_optimistic_blocks)
            && self.dry_run_validation_url.is_empty()
        {
            eyre::bail!(
                "Dry run or optimistic prevalidation enabled but no validation urls provided"
            );
        }
        let validation_api = {
            let urls = self
                .dry_run_validation_url
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>();

            ValidationAPIClient::new(urls.as_slice())?
        };

        let optimistic_signer = match self.bls_optimistic_signer(&chain_spec) {
            Ok(signer) => signer,
            Err(err) => {
                if self.optimistic_enabled {
                    eyre::bail!(
                        "Optimistic mode enabled but no valid optimistic signer: {}",
                        err
                    );
                } else {
                    // we don't care about the actual value
                    self.bls_signer(&chain_spec)?
                }
            }
        };

        let signer = self.bls_signer(&chain_spec)?;

        Ok(SubmissionConfig {
            chain_spec: chain_spec,
            signer: signer,
            dry_run: self.dry_run,
            validation_api,
            optimistic_enabled: self.optimistic_enabled,
            optimistic_signer,
            optimistic_max_bid_value: parse_ether(&self.optimistic_max_bid_value_eth)?,
            optimistic_prevalidate_optimistic_blocks: self.optimistic_prevalidate_optimistic_blocks,
            blocks_processor: if let Some(url) = &self.blocks_processor_url {
                let client = BlocksProcessorClient::try_from(url)?;
                Some(client)
            } else {
                None
            },
            slot_delta_to_start_submits: self.slot_delta_to_start_submits(),
        })
    }
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
        super::LiveBuilder<
            Arc<DatabaseEnv>,
            super::building::relay_submit::RelaySubmitSinkFactory,
            MevBoostSlotDataGenerator,
        >,
    > {
        let submission_config = self
            .l1_config
            .submission_config(self.base_config.chain_spec()?)?;
        info!(
            "Builder mev boost normal relay pubkey: {:?}",
            submission_config.signer.pub_key()
        );
        info!(
            "Builder mev boost optimistic relay pubkey: {:?}",
            submission_config.optimistic_signer.pub_key()
        );
        info!(
            "Optimistic mode, enabled: {}, prevalidate: {}, max_value: {}",
            submission_config.optimistic_enabled,
            submission_config.optimistic_prevalidate_optimistic_blocks,
            format_ether(submission_config.optimistic_max_bid_value),
        );

        let relays = self.l1_config.relays()?;
        let sink_factory = RelaySubmitSinkFactory::new(submission_config, relays.clone());

        let payload_event = MevBoostSlotDataGenerator::new(
            self.l1_config.beacon_clients()?,
            relays,
            self.base_config.blocklist()?,
            cancellation_token.clone(),
        );

        let live_builder = self
            .base_config
            .create_builder(cancellation_token, sink_factory, payload_event)
            .await?;
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
            l1_config: Default::default(),
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

fn get_signing_domain(chain: Chain, beacon_clients: Vec<Client>) -> eyre::Result<B256> {
    let cl_context = match chain.kind() {
        ChainKind::Named(NamedChain::Mainnet) => ContextEth::for_mainnet(),
        ChainKind::Named(NamedChain::Sepolia) => ContextEth::for_sepolia(),
        ChainKind::Named(NamedChain::Goerli) => ContextEth::for_goerli(),
        ChainKind::Named(NamedChain::Holesky) => ContextEth::for_holesky(),
        _ => {
            let client = beacon_clients
                .first()
                .ok_or_else(|| eyre::eyre!("No beacon clients provided"))?;

            let spec = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(client.get_spec())
            })?;

            let genesis_fork_version = spec
                .get("GENESIS_FORK_VERSION")
                .ok_or_else(|| eyre::eyre!("GENESIS_FORK_VERSION not found in spec"))?;

            let version: FixedBytes<4> = FixedBytes::from_str(genesis_fork_version)
                .map_err(|e| eyre::eyre!("Failed to parse genesis fork version: {:?}", e))?;

            let version = Version::from(version);

            // use the mainnet one and update the genesis fork version since it is the
            // only thing required by 'compute_builder_domain'. We do this because
            // there is no default in Context.
            let mut network = ContextEth::for_mainnet();
            network.genesis_fork_version = version;

            network
        }
    };

    Ok(B256::from(&compute_builder_domain(&cl_context)?))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::live_builder::base_config::load_config_toml_and_env;
    use alloy_primitives::{address, fixed_bytes};
    use std::env;
    use url::Url;

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
            .l1_config
            .resolve_cl_node_urls()
            .unwrap()
            .contains(&"http://localhost:3500".to_string()));
    }

    #[test]
    fn test_parse_backtest_example_config() {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("../../config-backtest-example.toml");

        load_config_toml_and_env::<Config>(p).expect("Config load");
    }

    #[test]
    fn test_signing_domain_known_chains() {
        let cases = [
            (
                NamedChain::Mainnet,
                fixed_bytes!("00000001f5a5fd42d16a20302798ef6ed309979b43003d2320d9f0e8ea9831a9"),
            ),
            (
                NamedChain::Sepolia,
                fixed_bytes!("00000001d3010778cd08ee514b08fe67b6c503b510987a4ce43f42306d97c67c"),
            ),
            (
                NamedChain::Goerli,
                fixed_bytes!("00000001e4be9393b074ca1f3e4aabd585ca4bea101170ccfaf71b89ce5c5c38"),
            ),
            (
                NamedChain::Holesky,
                fixed_bytes!("000000015b83a23759c560b2d0c64576e1dcfc34ea94c4988f3e0d9f77f05387"),
            ),
        ];

        for (chain, domain) in cases.iter() {
            let found = get_signing_domain(Chain::from_named(*chain), vec![]).unwrap();
            assert_eq!(found, *domain);
        }
    }

    #[ignore]
    #[test]
    fn test_signing_domain_custom_chain() {
        let client = Client::new(Url::parse("http://localhost:8000").unwrap());
        let found = get_signing_domain(Chain::from_id(12345), vec![client]).unwrap();

        assert_eq!(
            found,
            fixed_bytes!("00000001aaf2630a2874a74199f4b5d11a7d6377f363a236271bff4bf8eb4ab3")
        );
    }
}
