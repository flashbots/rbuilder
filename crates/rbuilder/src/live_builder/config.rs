//! Config should always be deserializable, default values should be used
//!
//!
use super::{
    base_config::BaseConfig,
    block_output::{
        bid_observer::{BidObserver, NullBidObserver},
        bid_value_source::null_bid_value_source::NullBidValueSource,
        bidding::{
            interfaces::BiddingService, true_block_value_bidder::TrueBlockValueBiddingService,
            wallet_balance_watcher::WalletBalanceWatcher,
        },
        block_sealing_bidder_factory::BlockSealingBidderFactory,
        relay_submit::{OptimisticConfig, RelaySubmitSinkFactory, SubmissionConfig},
    },
};
use crate::{
    beacon_api_client::Client,
    building::{
        builders::{
            ordering_builder::{OrderingBuilderConfig, OrderingBuildingAlgorithm},
            parallel_builder::{
                parallel_build_backtest, ParallelBuilderConfig, ParallelBuildingAlgorithm,
            },
            BacktestSimulateBlockInput, Block, BlockBuildingAlgorithm,
        },
        Sorting,
    },
    live_builder::{
        base_config::EnvOrValue, block_output::relay_submit::BuilderSinkFactory,
        cli::LiveBuilderConfig, payload_events::MevBoostSlotDataGenerator,
    },
    mev_boost::BLSBlockSigner,
    primitives::mev_boost::{MevBoostRelay, RelayConfig},
    roothash::RootHashConfig,
    utils::{build_info::rbuilder_version, ProviderFactoryReopener, Signer},
    validation_api_client::ValidationAPIClient,
};
use alloy_chains::ChainKind;
use alloy_primitives::{
    utils::{format_ether, parse_ether},
    FixedBytes, B256,
};
use ethereum_consensus::{
    builder::compute_builder_domain, crypto::SecretKey, primitives::Version,
    state_transition::Context as ContextEth,
};
use eyre::Context;
use lazy_static::lazy_static;
use reth::revm::cached::CachedReads;
use reth_chainspec::{Chain, ChainSpec, NamedChain};
use reth_db::{Database, DatabaseEnv};
use reth_node_api::NodeTypesWithDBAdapter;
use reth_node_ethereum::EthereumNode;
use reth_primitives::StaticFileSegment;
use reth_provider::{
    BlockReader, DatabaseProviderFactory, HeaderProvider, StateProviderFactory,
    StaticFileProviderFactory,
};
use serde::Deserialize;
use serde_with::{serde_as, OneOrMany};
use std::collections::HashMap;
use std::{
    fmt::Debug,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tracing::{info, warn};
use url::Url;

/// We initialize the wallet with the last full day. This should be enough for any bidder.
/// On debug I measured this to be < 300ms so it's not big deal.
pub const WALLET_INIT_HISTORY_SIZE: Duration = Duration::from_secs(60 * 60 * 24);
/// 1 is easier for debugging.
pub const DEFAULT_MAX_CONCURRENT_SEALS: u64 = 1;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "algo", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SpecificBuilderConfig {
    ParallelBuilder(ParallelBuilderConfig),
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
    pub enabled_relays: Vec<String>,

    pub dry_run: bool,
    #[serde_as(deserialize_as = "OneOrMany<_>")]
    pub dry_run_validation_url: Vec<String>,
    /// Secret key that will be used to sign normal submissions to the relay.
    relay_secret_key: Option<EnvOrValue<String>>,
    /// Secret key that will be used to sign optimistic submissions to the relay.
    optimistic_relay_secret_key: EnvOrValue<String>,
    /// When enabled builer will make optimistic submissions to optimistic relays
    /// influenced by `optimistic_max_bid_value_eth` and `optimistic_prevalidate_optimistic_blocks`
    pub optimistic_enabled: bool,
    /// Bids above this value will always be submitted in non-optimistic mode.
    pub optimistic_max_bid_value_eth: String,
    /// If true all optimistic submissions will be validated on nodes specified in `dry_run_validation_url`
    pub optimistic_prevalidate_optimistic_blocks: bool,

    /// How many seals we are going to be doing in parallel.
    /// Optimal value may change depending on the roothash computation caching strategies.
    pub max_concurrent_seals: u64,

    ///Name kept singular for backwards compatibility
    #[serde_as(deserialize_as = "OneOrMany<EnvOrValue<String>>")]
    pub cl_node_url: Vec<EnvOrValue<String>>,

    /// Genesis fork version for the chain. If not provided it will be fetched from the beacon client.
    pub genesis_fork_version: Option<String>,
}

impl Default for L1Config {
    fn default() -> Self {
        Self {
            relays: vec![],
            enabled_relays: vec![],
            dry_run: false,
            dry_run_validation_url: vec![],
            relay_secret_key: None,
            optimistic_relay_secret_key: "".into(),
            optimistic_enabled: false,
            optimistic_max_bid_value_eth: "0.0".to_string(),
            optimistic_prevalidate_optimistic_blocks: false,
            cl_node_url: vec![EnvOrValue::from("http://127.0.0.1:3500")],
            max_concurrent_seals: DEFAULT_MAX_CONCURRENT_SEALS,
            genesis_fork_version: None,
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

    pub fn create_relays(&self) -> eyre::Result<Vec<MevBoostRelay>> {
        let mut relay_configs = DEFAULT_RELAYS.clone();

        // Update relay configs from user configuration - replace if found
        for relay in self.relays.clone() {
            relay_configs.insert(relay.name.clone(), relay);
        }

        // For backwards compatibility: add all user-configured relays to enabled_relays
        let mut effective_enabled_relays: std::collections::HashSet<String> =
            self.enabled_relays.iter().cloned().collect();
        effective_enabled_relays.extend(self.relays.iter().map(|r| r.name.clone()));

        // Create enabled relays
        let mut results = Vec::new();
        for relay_name in effective_enabled_relays.iter() {
            match relay_configs.get(relay_name) {
                Some(relay_config) => match MevBoostRelay::from_config(relay_config) {
                    Ok(relay) => {
                        info!(
                            "Created relay: {:?} (priority: {})",
                            relay_name, relay.priority
                        );
                        results.push(relay);
                    }
                    Err(e) => {
                        return Err(eyre::eyre!(
                            "Failed to create relay {}: {:?}",
                            relay_name,
                            e
                        ));
                    }
                },
                None => {
                    return Err(eyre::eyre!("Relay {} not found in relays list", relay_name));
                }
            }
        }

        if results.is_empty() {
            return Err(eyre::eyre!("No relays enabled"));
        }

        Ok(results)
    }

    fn submission_config(
        &self,
        chain_spec: Arc<ChainSpec>,
        bid_observer: Box<dyn BidObserver + Send + Sync>,
    ) -> eyre::Result<SubmissionConfig> {
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

        let signing_domain = get_signing_domain(
            chain_spec.chain,
            self.beacon_clients()?,
            self.genesis_fork_version.clone(),
        )?;

        let relay_secret_key = if let Some(secret_key) = &self.relay_secret_key {
            let resolved_key = secret_key.value()?;
            SecretKey::try_from(resolved_key)?
        } else {
            warn!("No relay secret key provided. A random key will be generated.");
            SecretKey::random(&mut rand::thread_rng())?
        };

        let signer = BLSBlockSigner::new(relay_secret_key, signing_domain)
            .map_err(|e| eyre::eyre!("Failed to create normal signer: {:?}", e))?;

        let optimistic_signer = if self.optimistic_enabled {
            BLSBlockSigner::from_string(self.optimistic_relay_secret_key.value()?, signing_domain)
                .map_err(|e| eyre::eyre!("Failed to create optimistic signer: {:?}", e))?
        } else {
            // Placeholder value since it is required for SubmissionConfig. But after https://github.com/flashbots/rbuilder/pull/323
            // we can return None
            signer.clone()
        };

        let optimistic_config = if self.optimistic_enabled {
            Some(OptimisticConfig {
                signer: optimistic_signer,
                max_bid_value: parse_ether(&self.optimistic_max_bid_value_eth)?,
                prevalidate_optimistic_blocks: self.optimistic_prevalidate_optimistic_blocks,
            })
        } else {
            None
        };

        Ok(SubmissionConfig {
            chain_spec,
            signer,
            dry_run: self.dry_run,
            validation_api,
            optimistic_config,
            bid_observer,
        })
    }

    /// Creates the RelaySubmitSinkFactory and also returns the associated relays.
    pub fn create_relays_sealed_sink_factory(
        &self,
        chain_spec: Arc<ChainSpec>,
        bid_observer: Box<dyn BidObserver + Send + Sync>,
    ) -> eyre::Result<(Box<dyn BuilderSinkFactory>, Vec<MevBoostRelay>)> {
        let submission_config = self.submission_config(chain_spec, bid_observer)?;
        info!(
            "Builder mev boost normal relay pubkey: {:?}",
            submission_config.signer.pub_key()
        );

        if let Some(optimitic_config) = submission_config.optimistic_config.as_ref() {
            info!(
                "Optimistic mode enabled, relay pubkey {:?}, prevalidate: {}, max_value: {}",
                optimitic_config.signer.pub_key(),
                optimitic_config.prevalidate_optimistic_blocks,
                format_ether(optimitic_config.max_bid_value),
            );
        };

        let relays = self.create_relays()?;
        if relays.is_empty() {
            eyre::bail!("No relays provided");
        }

        let sink_factory: Box<dyn BuilderSinkFactory> = Box::new(RelaySubmitSinkFactory::new(
            submission_config,
            relays.clone(),
        ));
        Ok((sink_factory, relays))
    }
}

impl LiveBuilderConfig for Config {
    fn base_config(&self) -> &BaseConfig {
        &self.base_config
    }
    async fn new_builder<P, DB>(
        &self,
        provider: P,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> eyre::Result<super::LiveBuilder<P, DB, MevBoostSlotDataGenerator>>
    where
        DB: Database + Clone + 'static,
        P: DatabaseProviderFactory<DB = DB, Provider: BlockReader>
            + StateProviderFactory
            + HeaderProvider
            + Clone
            + 'static,
    {
        let (sink_sealed_factory, relays) = self.l1_config.create_relays_sealed_sink_factory(
            self.base_config.chain_spec()?,
            Box::new(NullBidObserver {}),
        )?;

        let (wallet_balance_watcher, wallet_history) = WalletBalanceWatcher::new(
            provider.clone(),
            self.base_config.coinbase_signer()?.address,
            WALLET_INIT_HISTORY_SIZE,
        )?;
        let bidding_service: Box<dyn BiddingService> =
            Box::new(TrueBlockValueBiddingService::new(&wallet_history));

        let sink_factory = Box::new(BlockSealingBidderFactory::new(
            bidding_service,
            sink_sealed_factory,
            Arc::new(NullBidValueSource {}),
            wallet_balance_watcher,
            self.l1_config.max_concurrent_seals as usize,
        ));

        let payload_event = MevBoostSlotDataGenerator::new(
            self.l1_config.beacon_clients()?,
            relays,
            self.base_config.blocklist()?,
            cancellation_token.clone(),
        );
        let live_builder = self
            .base_config
            .create_builder_with_provider_factory(
                cancellation_token,
                sink_factory,
                payload_event,
                provider,
            )
            .await?;
        let root_hash_config = self.base_config.live_root_hash_config()?;
        let builders = create_builders(self.live_builders()?, root_hash_config);
        Ok(live_builder.with_builders(builders))
    }

    fn version_for_telemetry(&self) -> crate::utils::build_info::Version {
        rbuilder_version()
    }

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
            + 'static,
    {
        let builder_cfg = self.builder(building_algorithm_name)?;
        match builder_cfg.builder {
            SpecificBuilderConfig::OrderingBuilder(config) => {
                crate::building::builders::ordering_builder::backtest_simulate_block(config, input)
            }
            SpecificBuilderConfig::ParallelBuilder(config) => {
                parallel_build_backtest::<P, DB>(input, config)
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
                BuilderConfig {
                    name: String::from("mp-ordering-deadline"),
                    builder: SpecificBuilderConfig::OrderingBuilder(OrderingBuilderConfig {
                        discard_txs: true,
                        sorting: Sorting::MaxProfit,
                        failed_order_retries: 1,
                        drop_failed_orders: true,
                        coinbase_payment: false,
                        build_duration_deadline_ms: Some(30),
                    }),
                },
                BuilderConfig {
                    name: String::from("mp-ordering-cb"),
                    builder: SpecificBuilderConfig::OrderingBuilder(OrderingBuilderConfig {
                        discard_txs: true,
                        sorting: Sorting::MaxProfit,
                        failed_order_retries: 1,
                        drop_failed_orders: true,
                        coinbase_payment: true,
                        build_duration_deadline_ms: None,
                    }),
                },
                BuilderConfig {
                    name: String::from("mgp-ordering-default"),
                    builder: SpecificBuilderConfig::OrderingBuilder(OrderingBuilderConfig {
                        discard_txs: true,
                        sorting: Sorting::MevGasPrice,
                        failed_order_retries: 1,
                        drop_failed_orders: false,
                        coinbase_payment: false,
                        build_duration_deadline_ms: None,
                    }),
                },
                BuilderConfig {
                    name: String::from("parallel"),
                    builder: SpecificBuilderConfig::ParallelBuilder(ParallelBuilderConfig {
                        discard_txs: true,
                        num_threads: 25,
                        coinbase_payment: false,
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
) -> eyre::Result<ProviderFactoryReopener<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>> {
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

pub fn create_builders<P, DB>(
    configs: Vec<BuilderConfig>,
    root_hash_config: RootHashConfig,
) -> Vec<Arc<dyn BlockBuildingAlgorithm<P, DB>>>
where
    DB: Database + Clone + 'static,
    P: DatabaseProviderFactory<DB = DB, Provider: BlockReader>
        + StateProviderFactory
        + Clone
        + 'static,
{
    configs
        .into_iter()
        .map(|cfg| create_builder(cfg, &root_hash_config))
        .collect()
}

fn create_builder<P, DB>(
    cfg: BuilderConfig,
    root_hash_config: &RootHashConfig,
) -> Arc<dyn BlockBuildingAlgorithm<P, DB>>
where
    DB: Database + Clone + 'static,
    P: DatabaseProviderFactory<DB = DB, Provider: BlockReader>
        + StateProviderFactory
        + Clone
        + 'static,
{
    match cfg.builder {
        SpecificBuilderConfig::OrderingBuilder(order_cfg) => Arc::new(
            OrderingBuildingAlgorithm::new(root_hash_config.clone(), order_cfg, cfg.name),
        ),
        SpecificBuilderConfig::ParallelBuilder(parallel_cfg) => Arc::new(
            ParallelBuildingAlgorithm::new(root_hash_config.clone(), parallel_cfg, cfg.name),
        ),
    }
}

fn get_signing_domain(
    chain: Chain,
    beacon_clients: Vec<Client>,
    genesis_fork_version: Option<String>,
) -> eyre::Result<B256> {
    let cl_context = match chain.kind() {
        ChainKind::Named(NamedChain::Mainnet) => ContextEth::for_mainnet(),
        ChainKind::Named(NamedChain::Sepolia) => ContextEth::for_sepolia(),
        ChainKind::Named(NamedChain::Goerli) => ContextEth::for_goerli(),
        ChainKind::Named(NamedChain::Holesky) => ContextEth::for_holesky(),
        _ => {
            let genesis_fork_version = if let Some(genesis_fork_version) = genesis_fork_version {
                genesis_fork_version
            } else {
                let client = beacon_clients
                    .first()
                    .ok_or_else(|| eyre::eyre!("No beacon clients provided"))?;

                let spec = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(client.get_spec())
                })?;

                spec.get("GENESIS_FORK_VERSION")
                    .ok_or_else(|| eyre::eyre!("GENESIS_FORK_VERSION not found in spec"))?
                    .clone()
            };

            let version: FixedBytes<4> = FixedBytes::from_str(&genesis_fork_version)
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

lazy_static! {
    static ref DEFAULT_RELAYS: HashMap<String, RelayConfig> = {
        let mut map = HashMap::new();
        map.insert(
            "flashbots".to_string(),
            RelayConfig {
                name: "flashbots".to_string(),
                url: "http://k8s-default-boostrel-9f278153f5-947835446.us-east-2.elb.amazonaws.com"
                    .to_string(),
                use_ssz_for_submit: true,
                use_gzip_for_submit: false,
                priority: 0,
                optimistic: false,
                interval_between_submissions_ms: Some(250),
                authorization_header: None,
                builder_id_header: None,
                api_token_header: None,
            },
        );
        map.insert(
            "ultrasound-us".to_string(),
            RelayConfig {
                name: "ultrasound-us".to_string(),
                url: "https://relay-builders-us.ultrasound.money".to_string(),
                use_ssz_for_submit: true,
                use_gzip_for_submit: true,
                priority: 0,
                optimistic: true,
                interval_between_submissions_ms: None,
                authorization_header: None,
                builder_id_header: None,
                api_token_header: None,
            },
        );
        map.insert(
            "ultrasound-eu".to_string(),
            RelayConfig {
                name: "ultrasound-eu".to_string(),
                url: "https://relay-builders-eu.ultrasound.money".to_string(),
                use_ssz_for_submit: true,
                use_gzip_for_submit: true,
                priority: 0,
                optimistic: true,
                interval_between_submissions_ms: None,
                authorization_header: None,
                builder_id_header: None,
                api_token_header: None,
            },
        );
        map.insert(
            "agnostic".to_string(),
            RelayConfig {
                name: "agnostic".to_string(),
                url: "https://0xa7ab7a996c8584251c8f925da3170bdfd6ebc75d50f5ddc4050a6fdc77f2a3b5fce2cc750d0865e05d7228af97d69561@agnostic-relay.net".to_string(),
                use_ssz_for_submit: true,
                use_gzip_for_submit: true,
                priority: 0,
                optimistic: true,
                interval_between_submissions_ms: None,
                authorization_header: None,
                builder_id_header: None,
                api_token_header: None,
            },
        );
        map.insert(
            "playground".to_string(),
            RelayConfig {
                name: "playground".to_string(),
                url: "http://0xac6e77dfe25ecd6110b8e780608cce0dab71fdd5ebea22a16c0205200f2f8e2e3ad3b71d3499c54ad14d6c21b41a37ae@localhost:5555".to_string(),
                priority: 0,
                use_ssz_for_submit: false,
                use_gzip_for_submit: false,
                optimistic: false,
                interval_between_submissions_ms: None,
                authorization_header: None,
                builder_id_header: None,
                api_token_header: None,
            },
        );
        map
    };
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
    fn test_parse_enabled_relays() {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("./src/live_builder/testdata/config_with_relay_override.toml");

        let config: Config = load_config_toml_and_env(p.clone()).expect("Config load");

        let relays = config.l1_config.create_relays().unwrap();
        assert_eq!(relays.len(), 1);
        assert_eq!(relays[0].id, "playground");
        assert_eq!(relays[0].priority, 10);
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
            let found = get_signing_domain(Chain::from_named(*chain), vec![], None).unwrap();
            assert_eq!(found, *domain);
        }
    }

    #[test]
    fn test_signing_domain_with_genesis_fork() {
        let client = Client::new(Url::parse("http://localhost:8000").unwrap());
        let found = get_signing_domain(
            Chain::from_id(12345),
            vec![client],
            Some("0x00112233".to_string()),
        )
        .unwrap();

        assert_eq!(
            found,
            fixed_bytes!("0000000157eb3d0fd9a819dee70b5403ce939a22b4f25ec3fc841a16cc4eab3e")
        );
    }

    #[ignore]
    #[test]
    fn test_signing_domain_custom_chain() {
        let client = Client::new(Url::parse("http://localhost:8000").unwrap());
        let found = get_signing_domain(Chain::from_id(12345), vec![client], None).unwrap();

        assert_eq!(
            found,
            fixed_bytes!("00000001aaf2630a2874a74199f4b5d11a7d6377f363a236271bff4bf8eb4ab3")
        );
    }
}
