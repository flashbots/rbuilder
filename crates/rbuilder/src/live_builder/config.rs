//! Config should always be deserializable, default values should be used
//!
//!
use super::{
    base_config::BaseConfig,
    block_output::{
        bidding_service_interface::{
            BidObserver, BiddingService, LandedBlockInfo, NullBidObserver,
        },
        relay_submit::{RelaySubmitSinkFactory, SubmissionConfig},
        true_value_bidding_service::NewTrueBlockValueBiddingService,
        unfinished_block_processing::UnfinishedBuiltBlocksInputFactory,
    },
    wallet_balance_watcher::WalletBalanceWatcher,
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
        order_priority::{
            FullProfitInfoGetter, NonMempoolProfitInfoGetter, OrderLengthThreeMaxProfitPriority,
            OrderLengthThreeMevGasPricePriority, OrderMaxProfitPriority, OrderMevGasPricePriority,
            OrderTypePriority, ProfitInfoGetter,
        },
        PartialBlockExecutionTracer, Sorting,
    },
    live_builder::{
        base_config::default_ip,
        block_output::{
            bidding_service_interface::{BiddingService2BidSender, RelaySet},
            relay_submit::OptimisticV3Config,
        },
        cli::LiveBuilderConfig,
        payload_events::MevBoostSlotDataGenerator,
    },
    mev_boost::{
        bloxroute_grpc,
        optimistic_v3::{self, OPTIMISTIC_V3_CHANNEL_SIZE},
        BLSBlockSigner, MevBoostRelayBidSubmitter, MevBoostRelaySlotInfoProvider, RelayClient,
        RelayConfig, RelaySubmitConfig,
    },
    provider::StateProviderFactory,
    roothash::RootHashContext,
    utils::{build_info::rbuilder_version, ProviderFactoryReopener, Signer},
};
use alloy_chains::ChainKind;
use alloy_primitives::{utils::parse_ether, Address, FixedBytes, B256, U256};
use alloy_rpc_types_beacon::BlsPublicKey;
use bid_scraper::config::NamedPublisherConfig;
use ethereum_consensus::{
    builder::compute_builder_domain, crypto::SecretKey, primitives::Version,
    state_transition::Context as ContextEth,
};
use eyre::Context;
use lazy_static::lazy_static;
use rbuilder_config::EnvOrValue;
use rbuilder_primitives::mev_boost::{MevBoostRelayID, RelayMode};
use reth_chainspec::{Chain, ChainSpec, NamedChain};
use reth_db::DatabaseEnv;
use reth_node_api::NodeTypesWithDBAdapter;
use reth_node_ethereum::EthereumNode;
use reth_primitives::StaticFileSegment;
use reth_provider::StaticFileProviderFactory;
use serde::Deserialize;
use serde_with::{serde_as, OneOrMany};
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{broadcast, Mutex as TokioMutex};
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

/// We initialize the wallet with the last full day. This should be enough for any bidder.
/// On debug I measured this to be < 300ms so it's not big deal.
pub const WALLET_INIT_HISTORY_SIZE: Duration = Duration::from_secs(60 * 60 * 24);
/// 1 is easier for debugging.
pub const DEFAULT_MAX_CONCURRENT_SEALS: u64 = 1;

/// More than 2 blocks. This could happen normally every 1000 blocks approx since there is a 10% chance of non-boost blocks.
pub const BID_SOURCE_TIMEOUT_SECS: u64 = 28;
/// Don't want to waste too much time in case i failed to non-boost block.
pub const BID_SOURCE_WAIT_TIME_SECS: u64 = 2;

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

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SubsidyConfig {
    pub relay: MevBoostRelayID,
    pub value: String,
}

#[serde_as]
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    #[serde(flatten)]
    pub base_config: BaseConfig,

    #[serde(flatten)]
    pub l1_config: L1Config,

    /// selected builder configurations
    pub builders: Vec<BuilderConfig>,

    /// When the sample bidder (see TrueBlockValueBiddingService) will start bidding.
    /// Usually a negative number.
    pub slot_delta_to_start_bidding_ms: Option<i64>,
    /// Value added to the bids (see TrueBlockValueBiddingService).
    pub subsidy: Option<String>,
    /// Overrides subsidy.
    #[serde(default)]
    pub subsidy_overrides: Vec<SubsidyConfig>,
}

const DEFAULT_SLOT_DELTA_TO_START_BIDDING_MS: i64 = -8000;
const DEFAULT_REGISTRATION_UPDATE_INTERVAL_MS: u64 = 5_000;
const DEFAULT_ASK_FOR_FILTERING_VALIDATORS: bool = false;
const DEFAULT_CAN_IGNORE_GAS_LIMIT: bool = false;

#[serde_as]
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct L1Config {
    // Relay Submission configuration
    pub relays: Vec<RelayConfig>,
    pub enabled_relays: Vec<String>,
    /// The interval at which validator registrations should be updated.
    pub registration_update_interval_ms: Option<u64>,
    /// Secret key that will be used to sign normal submissions to the relay.
    relay_secret_key: Option<EnvOrValue<String>>,

    /// Name kept singular for backwards compatibility
    #[serde_as(deserialize_as = "OneOrMany<EnvOrValue<String>>")]
    pub cl_node_url: Vec<EnvOrValue<String>>,

    /// Genesis fork version for the chain. If not provided it will be fetched from the beacon client.
    pub genesis_fork_version: Option<String>,
    /// A bid scraper will be spawned for each NamedPublisherConfig.
    pub relay_bid_scrapers: Vec<NamedPublisherConfig>,

    /// Optimistic V3 server IP.
    #[serde(default = "default_ip")]
    pub optimistic_v3_server_ip: Ipv4Addr,
    /// Optimistic V3 server port.
    pub optimistic_v3_server_port: u16,
    /// Optimistic V3 public URL.
    pub optimistic_v3_public_url: String,
    /// The relay pubkey.
    pub optimistic_v3_relay_pubkeys: HashSet<BlsPublicKey>,
}

impl Default for L1Config {
    fn default() -> Self {
        Self {
            relays: vec![],
            enabled_relays: vec![],
            relay_secret_key: None,
            cl_node_url: vec![EnvOrValue::from("http://127.0.0.1:3500")],
            genesis_fork_version: None,
            relay_bid_scrapers: Default::default(),
            registration_update_interval_ms: None,
            optimistic_v3_server_ip: default_ip(),
            optimistic_v3_server_port: 6071,
            optimistic_v3_public_url: String::new(),
            optimistic_v3_relay_pubkeys: HashSet::default(),
        }
    }
}

impl L1Config {
    pub fn resolve_cl_node_urls(&self) -> eyre::Result<Vec<String>> {
        rbuilder_config::resolve_env_or_values::<String>(&self.cl_node_url)
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

    /// Analyzes relay_config and creates MevBoostRelayBidSubmitter/MevBoostRelaySlotInfoProvider as needed.
    fn create_relay_sub_objects(
        relay_config: &RelayConfig,
        client: RelayClient,
        submitters: &mut Vec<MevBoostRelayBidSubmitter>,
        slot_info_providers: &mut Vec<MevBoostRelaySlotInfoProvider>,
    ) -> eyre::Result<()> {
        if relay_config.priority.is_some() {
            warn!(
                relay = relay_config.name,
                "Deprecated: relay priority set, ignoring"
            );
        }

        if relay_config.mode.submits_bids() {
            if let Some(submit_config) = &relay_config.submit_config {
                submitters.push(MevBoostRelayBidSubmitter::new(
                    client.clone(),
                    relay_config.name.clone(),
                    submit_config,
                    relay_config.mode == RelayMode::Test,
                )?);
            } else {
                eyre::bail!(
                    "Relay {} in mode {:?} has no submit config",
                    relay_config.name,
                    relay_config.mode
                );
            }
        }
        if relay_config.mode.gets_slot_info() {
            slot_info_providers.push(MevBoostRelaySlotInfoProvider::new(
                client.clone(),
                relay_config.name.clone(),
            ));
        }
        Ok(())
    }

    pub fn relays_ids(&self) -> RelaySet {
        let mut effective_enabled_relays: std::collections::HashSet<MevBoostRelayID> =
            self.enabled_relays.iter().cloned().collect();
        effective_enabled_relays.extend(self.relays.iter().map(|r| r.name.clone()));
        effective_enabled_relays
            .into_iter()
            .collect::<Vec<MevBoostRelayID>>()
            .into()
    }

    pub fn create_relays(
        &self,
    ) -> eyre::Result<(
        Vec<MevBoostRelayBidSubmitter>,
        Vec<MevBoostRelaySlotInfoProvider>,
    )> {
        let mut relay_configs = DEFAULT_RELAYS.clone();
        // Update relay configs from user configuration - replace if found
        for relay in self.relays.clone() {
            relay_configs.insert(relay.name.clone(), relay);
        }
        // For backwards compatibility: add all user-configured relays to enabled_relays
        let effective_enabled_relays = self.relays_ids();
        // Create enabled relays
        let mut submitters = Vec::new();
        let mut slot_info_providers = Vec::new();
        for relay_name in effective_enabled_relays.relays().iter() {
            match relay_configs.get(relay_name) {
                Some(relay_config) => {
                    let url = match relay_config.url.parse() {
                        Ok(url) => url,
                        Err(err) => {
                            eyre::bail!(
                                "Failed to parse relay url. Error = {err}. Url = {}",
                                relay_config.url
                            );
                        }
                    };
                    let mut client = RelayClient::from_url(
                        url,
                        relay_config.authorization_header.clone(),
                        relay_config.builder_id_header.clone(),
                        relay_config.api_token_header.clone(),
                        relay_config.is_bloxroute,
                        relay_config.bloxroute_rproxy_regions.clone(),
                        relay_config.bloxroute_rproxy_only,
                        relay_config
                            .ask_for_filtering_validators
                            .unwrap_or(DEFAULT_ASK_FOR_FILTERING_VALIDATORS),
                        relay_config
                            .can_ignore_gas_limit
                            .unwrap_or(DEFAULT_CAN_IGNORE_GAS_LIMIT),
                    );
                    if let Some(grpc_url) = relay_config.grpc_url.clone() {
                        let grpc_client = Arc::new(TokioMutex::new(
                            bloxroute_grpc::types::relay_client::RelayClient::new(
                                tonic::transport::Endpoint::try_from(grpc_url)?.connect_lazy(),
                            ),
                        ));
                        client = client.with_grpc_client(grpc_client);
                    }
                    Self::create_relay_sub_objects(
                        relay_config,
                        client,
                        &mut submitters,
                        &mut slot_info_providers,
                    )?;
                }
                None => {
                    return Err(eyre::eyre!("Relay {} not found in relays list", relay_name));
                }
            }
        }
        if slot_info_providers.is_empty() {
            return Err(eyre::eyre!("No relays enabled for getting slot info"));
        }
        Ok((submitters, slot_info_providers))
    }

    fn submission_config(
        &self,
        chain_spec: Arc<ChainSpec>,
        signing_domain: B256,
        bid_observer: Box<dyn BidObserver + Send + Sync>,
        optimistic_v3_config: Option<OptimisticV3Config>,
    ) -> eyre::Result<SubmissionConfig> {
        let relay_secret_key = if let Some(secret_key) = &self.relay_secret_key {
            let resolved_key = secret_key.value()?;
            SecretKey::try_from(resolved_key)?
        } else {
            warn!("No relay secret key provided. A random key will be generated.");
            SecretKey::random(&mut rand::thread_rng())?
        };

        let signer = BLSBlockSigner::new(relay_secret_key, signing_domain)
            .map_err(|e| eyre::eyre!("Failed to create normal signer: {:?}", e))?;

        Ok(SubmissionConfig {
            chain_spec,
            signer,
            optimistic_v3_config,
            bid_observer,
        })
    }

    /// Creates the RelaySubmitSinkFactory and also returns the associated relays (MevBoostRelaySlotInfoProvider).
    #[allow(clippy::type_complexity)]
    pub fn create_relays_sealed_sink_factory(
        &self,
        chain_spec: Arc<ChainSpec>,
        relay_sets: Vec<RelaySet>,
        bid_observer: Box<dyn BidObserver + Send + Sync>,
    ) -> eyre::Result<(
        RelaySubmitSinkFactory,
        Vec<MevBoostRelaySlotInfoProvider>,
        ahash::HashMap<MevBoostRelayID, Address>,
    )> {
        let signing_domain = get_signing_domain(
            chain_spec.chain,
            self.beacon_clients()?,
            self.genesis_fork_version.clone(),
        )?;

        let mut optimistic_v3_config = None;
        if self
            .relays
            .iter()
            .any(|r| r.submit_config.as_ref().is_some_and(|c| c.optimistic_v3))
        {
            let address = SocketAddr::V4(SocketAddrV4::new(
                self.optimistic_v3_server_ip,
                self.optimistic_v3_server_port,
            ));
            let builder_url = self.optimistic_v3_public_url.clone();

            info!(local = %address, %builder_url, "Optimistic V3 is enabled for at least one relay, spawning server");
            if self.optimistic_v3_relay_pubkeys.is_empty() {
                warn!("Optimistic V3 is enabled, but no relay pubkeys have been configured");
            }

            let (optimistic_v3_block_tx, optimistic_v3_block_rx) =
                broadcast::channel(OPTIMISTIC_V3_CHANNEL_SIZE);
            optimistic_v3::spawn_server(
                address,
                signing_domain,
                self.optimistic_v3_relay_pubkeys.clone(),
                BroadcastStream::from(optimistic_v3_block_rx),
            )?;

            optimistic_v3_config = Some(OptimisticV3Config {
                builder_url: builder_url.into_bytes(),
                block_sender: optimistic_v3_block_tx,
            })
        }

        let submission_config = self.submission_config(
            chain_spec,
            signing_domain,
            bid_observer,
            optimistic_v3_config,
        )?;
        info!(
            "Builder mev boost relay pubkey: {:?}",
            submission_config.signer.pub_key()
        );

        let (submitters, slot_info_providers) = self.create_relays()?;
        if slot_info_providers.is_empty() {
            eyre::bail!("No slot info providers provided");
        }

        let sink_factory =
            RelaySubmitSinkFactory::new(submission_config, submitters.clone(), relay_sets);

        let adjustment_fee_payers = self
            .relays
            .iter()
            .filter_map(|r| {
                r.adjustment_fee_payer
                    .map(|fee_payer| (r.name.clone(), fee_payer))
            })
            .collect();

        Ok((sink_factory, slot_info_providers, adjustment_fee_payers))
    }

    pub fn registration_update_interval(&self) -> Duration {
        Duration::from_millis(
            self.registration_update_interval_ms
                .unwrap_or(DEFAULT_REGISTRATION_UPDATE_INTERVAL_MS),
        )
    }
}

impl LiveBuilderConfig for Config {
    fn base_config(&self) -> &BaseConfig {
        &self.base_config
    }

    async fn new_builder<P>(
        &self,
        provider: P,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> eyre::Result<super::LiveBuilder<P>>
    where
        P: StateProviderFactory + Clone + 'static,
    {
        let subsidy = self.subsidy.clone();
        let slot_delta_to_start_bidding_ms = time::Duration::milliseconds(
            self.slot_delta_to_start_bidding_ms
                .unwrap_or(DEFAULT_SLOT_DELTA_TO_START_BIDDING_MS),
        );

        let all_relays_set = self.l1_config.relays_ids();
        let mut subsidy_overrides = HashMap::default();
        for subsidy_override in self.subsidy_overrides.iter() {
            subsidy_overrides.insert(
                subsidy_override.relay.clone(),
                parse_ether(&subsidy_override.value)?,
            );
        }
        let bidding_service = Arc::new(NewTrueBlockValueBiddingService::new(
            subsidy
                .as_ref()
                .map(|s| parse_ether(s))
                .unwrap_or(Ok(U256::ZERO))?,
            subsidy_overrides,
            slot_delta_to_start_bidding_ms,
            all_relays_set.clone(),
        ));

        let (wallet_balance_watcher, _) =
            create_wallet_balance_watcher(provider.clone(), &self.base_config).await?;

        let (sink_factory, slot_info_provider, adjustment_fee_payers) =
            create_sink_factory_and_relays(
                &self.base_config,
                &self.l1_config,
                bidding_service.relay_sets(),
                wallet_balance_watcher,
                Box::new(NullBidObserver {}),
                bidding_service,
                cancellation_token.clone(),
            )
            .await?;

        let live_builder = create_builder_from_sink(
            &self.base_config,
            &self.l1_config,
            provider,
            sink_factory,
            slot_info_provider,
            adjustment_fee_payers,
            cancellation_token,
        )
        .await?;
        let builders = create_builders(
            self.live_builders()?,
            self.base_config.max_order_execution_duration_warning(),
        );
        Ok(live_builder.with_builders(builders))
    }

    fn version_for_telemetry(&self) -> crate::utils::build_info::Version {
        rbuilder_version()
    }

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
        P: StateProviderFactory + Clone + 'static,
    {
        let builder_cfg = self.builder(building_algorithm_name)?;
        match builder_cfg.builder {
            SpecificBuilderConfig::OrderingBuilder(config) => {
                if config.ignore_mempool_profit_on_bundles {
                    build_backtest_block_ordering_builder::<
                        P,
                        NonMempoolProfitInfoGetter,
                        PartialBlockExecutionTracerType,
                    >(config, input, partial_block_execution_tracer)
                } else {
                    build_backtest_block_ordering_builder::<
                        P,
                        FullProfitInfoGetter,
                        PartialBlockExecutionTracerType,
                    >(config, input, partial_block_execution_tracer)
                }
            }
            SpecificBuilderConfig::ParallelBuilder(config) => {
                parallel_build_backtest::<P>(input, config)
            }
        }
    }
}

pub fn build_backtest_block_ordering_builder<
    P,
    ProfitInfoGetterType: ProfitInfoGetter + 'static,
    PartialBlockExecutionTracerType: PartialBlockExecutionTracer + Clone + Send + Sync + 'static,
>(
    config: OrderingBuilderConfig,
    input: BacktestSimulateBlockInput<'_, P>,
    partial_block_execution_tracer: PartialBlockExecutionTracerType,
) -> eyre::Result<Block>
where
    P: StateProviderFactory + Clone + 'static,
{
    match config.sorting {
        Sorting::MevGasPrice => {
            crate::building::builders::ordering_builder::backtest_simulate_block::<
                P,
                OrderMevGasPricePriority<ProfitInfoGetterType>,
                PartialBlockExecutionTracerType,
            >(config, input, partial_block_execution_tracer)
        }
        Sorting::MaxProfit => {
            crate::building::builders::ordering_builder::backtest_simulate_block::<
                P,
                OrderMaxProfitPriority<ProfitInfoGetterType>,
                PartialBlockExecutionTracerType,
            >(config, input, partial_block_execution_tracer)
        }
        Sorting::TypeMaxProfit => {
            crate::building::builders::ordering_builder::backtest_simulate_block::<
                P,
                OrderTypePriority<ProfitInfoGetterType>,
                PartialBlockExecutionTracerType,
            >(config, input, partial_block_execution_tracer)
        }
        Sorting::LengthThreeMaxProfit => {
            crate::building::builders::ordering_builder::backtest_simulate_block::<
                P,
                OrderLengthThreeMaxProfitPriority<ProfitInfoGetterType>,
                PartialBlockExecutionTracerType,
            >(config, input, partial_block_execution_tracer)
        }
        Sorting::LengthThreeMevGasPrice => {
            crate::building::builders::ordering_builder::backtest_simulate_block::<
                P,
                OrderLengthThreeMevGasPricePriority<ProfitInfoGetterType>,
                PartialBlockExecutionTracerType,
            >(config, input, partial_block_execution_tracer)
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
                        build_duration_deadline_ms: None,
                        ignore_mempool_profit_on_bundles: false,
                        pre_filtered_build_duration_deadline_ms: Some(0),
                    }),
                },
                BuilderConfig {
                    name: "mp-ordering".to_string(),
                    builder: SpecificBuilderConfig::OrderingBuilder(OrderingBuilderConfig {
                        discard_txs: true,
                        sorting: Sorting::MaxProfit,
                        failed_order_retries: 1,
                        drop_failed_orders: true,
                        build_duration_deadline_ms: None,
                        ignore_mempool_profit_on_bundles: false,
                        pre_filtered_build_duration_deadline_ms: Some(0),
                    }),
                },
                BuilderConfig {
                    name: String::from("mp-ordering-deadline"),
                    builder: SpecificBuilderConfig::OrderingBuilder(OrderingBuilderConfig {
                        discard_txs: true,
                        sorting: Sorting::MaxProfit,
                        failed_order_retries: 1,
                        drop_failed_orders: true,
                        build_duration_deadline_ms: Some(30),
                        ignore_mempool_profit_on_bundles: false,
                        pre_filtered_build_duration_deadline_ms: Some(0),
                    }),
                },
                BuilderConfig {
                    name: String::from("mp-ordering-cb"),
                    builder: SpecificBuilderConfig::OrderingBuilder(OrderingBuilderConfig {
                        discard_txs: true,
                        sorting: Sorting::MaxProfit,
                        failed_order_retries: 1,
                        drop_failed_orders: true,
                        build_duration_deadline_ms: None,
                        ignore_mempool_profit_on_bundles: false,
                        pre_filtered_build_duration_deadline_ms: Some(0),
                    }),
                },
                BuilderConfig {
                    name: String::from("mgp-ordering-default"),
                    builder: SpecificBuilderConfig::OrderingBuilder(OrderingBuilderConfig {
                        discard_txs: true,
                        sorting: Sorting::MevGasPrice,
                        failed_order_retries: 1,
                        drop_failed_orders: false,
                        build_duration_deadline_ms: None,
                        ignore_mempool_profit_on_bundles: false,
                        pre_filtered_build_duration_deadline_ms: Some(0),
                    }),
                },
                BuilderConfig {
                    name: String::from("parallel"),
                    builder: SpecificBuilderConfig::ParallelBuilder(ParallelBuilderConfig {
                        discard_txs: true,
                        num_threads: 25,
                        safe_sorting_only: true,
                    }),
                },
            ],
            slot_delta_to_start_bidding_ms: None,
            subsidy: None,
            subsidy_overrides: Vec::new(),
        }
    }
}

/// Open reth db and DB should be opened once per process but it can be cloned and moved to different threads.
pub fn create_provider_factory(
    reth_datadir: Option<&Path>,
    reth_db_path: Option<&Path>,
    reth_static_files_path: Option<&Path>,
    chain_spec: Arc<ChainSpec>,
    root_hash_config: Option<RootHashContext>,
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
        ProviderFactoryReopener::new(db, chain_spec, reth_static_files_path, root_hash_config)?;

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

pub fn create_builders<P>(
    configs: Vec<BuilderConfig>,
    max_order_execution_duration_warning: Option<Duration>,
) -> Vec<Arc<dyn BlockBuildingAlgorithm<P>>>
where
    P: StateProviderFactory + Clone + 'static,
{
    configs
        .into_iter()
        .map(|cfg| create_builder(cfg, max_order_execution_duration_warning))
        .collect()
}

fn create_builder<P>(
    cfg: BuilderConfig,
    max_order_execution_duration_warning: Option<Duration>,
) -> Arc<dyn BlockBuildingAlgorithm<P>>
where
    P: StateProviderFactory + Clone + 'static,
{
    match cfg.builder {
        SpecificBuilderConfig::OrderingBuilder(order_cfg) => {
            if order_cfg.ignore_mempool_profit_on_bundles {
                create_ordering_builder::<P, NonMempoolProfitInfoGetter>(
                    order_cfg,
                    max_order_execution_duration_warning,
                    cfg.name,
                )
            } else {
                create_ordering_builder::<P, FullProfitInfoGetter>(
                    order_cfg,
                    max_order_execution_duration_warning,
                    cfg.name,
                )
            }
        }
        SpecificBuilderConfig::ParallelBuilder(parallel_cfg) => {
            Arc::new(ParallelBuildingAlgorithm::new(
                parallel_cfg,
                max_order_execution_duration_warning,
                cfg.name,
            ))
        }
    }
}

fn create_ordering_builder<P, ProfitInfoGetterType: ProfitInfoGetter + 'static>(
    cfg: OrderingBuilderConfig,
    max_order_execution_duration_warning: Option<Duration>,
    name: String,
) -> Arc<dyn BlockBuildingAlgorithm<P>>
where
    P: StateProviderFactory + Clone + 'static,
{
    match cfg.sorting {
        Sorting::MevGasPrice => Arc::new(OrderingBuildingAlgorithm::<
            OrderMevGasPricePriority<ProfitInfoGetterType>,
        >::new(
            cfg, max_order_execution_duration_warning, name
        )),
        Sorting::MaxProfit => Arc::new(OrderingBuildingAlgorithm::<
            OrderMaxProfitPriority<ProfitInfoGetterType>,
        >::new(
            cfg, max_order_execution_duration_warning, name
        )),
        Sorting::TypeMaxProfit => Arc::new(OrderingBuildingAlgorithm::<
            OrderTypePriority<ProfitInfoGetterType>,
        >::new(
            cfg, max_order_execution_duration_warning, name
        )),
        Sorting::LengthThreeMaxProfit => {
            Arc::new(OrderingBuildingAlgorithm::<
                OrderLengthThreeMaxProfitPriority<ProfitInfoGetterType>,
            >::new(
                cfg, max_order_execution_duration_warning, name
            ))
        }
        Sorting::LengthThreeMevGasPrice => {
            Arc::new(OrderingBuildingAlgorithm::<
                OrderLengthThreeMevGasPricePriority<ProfitInfoGetterType>,
            >::new(
                cfg, max_order_execution_duration_warning, name
            ))
        }
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
        ChainKind::Named(NamedChain::Hoodi) => ContextEth::for_hoodi(),
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
                grpc_url: None,
                mode: RelayMode::Full,
                submit_config: Some(RelaySubmitConfig {
                    use_ssz_for_submit: true,
                    use_gzip_for_submit: false,
                    optimistic: false,
                    interval_between_submissions_ms: Some(250),
                    max_bid_eth: None,
                    optimistic_v3: false,
                    optimistic_v3_bid_adjustment_required: false,
                }),
                priority: Some(0),
                authorization_header: None,
                builder_id_header: None,
                api_token_header: None,
                adjustment_fee_payer: None,
                is_bloxroute: false,
                bloxroute_rproxy_regions: Vec::new(),
                bloxroute_rproxy_only: false,
                ask_for_filtering_validators: None,
                can_ignore_gas_limit: None,
            },
        );
        map.insert(
            "ultrasound-us".to_string(),
            RelayConfig {
                name: "ultrasound-us".to_string(),
                url: "https://relay-builders-us.ultrasound.money".to_string(),
                grpc_url: None,
                mode: RelayMode::Full,
                submit_config: Some(RelaySubmitConfig {
                    use_ssz_for_submit: true,
                    use_gzip_for_submit: true,
                    optimistic: true,
                    interval_between_submissions_ms: None,
                    max_bid_eth: None,
                    optimistic_v3: false,
                    optimistic_v3_bid_adjustment_required: false,
                }),
                priority: Some(0),
                authorization_header: None,
                builder_id_header: None,
                api_token_header: None,
                adjustment_fee_payer: None,
                is_bloxroute: false,
                bloxroute_rproxy_regions: Vec::new(),
                bloxroute_rproxy_only: false,
                ask_for_filtering_validators: None,
                can_ignore_gas_limit: None,
            },
        );
        map.insert(
            "ultrasound-eu".to_string(),
            RelayConfig {
                name: "ultrasound-eu".to_string(),
                url: "https://relay-builders-eu.ultrasound.money".to_string(),
                grpc_url: None,
                mode: RelayMode::Full,
                submit_config: Some(RelaySubmitConfig {
                    use_ssz_for_submit: true,
                    use_gzip_for_submit: true,
                    optimistic: true,
                    interval_between_submissions_ms: None,
                    max_bid_eth: None,
                    optimistic_v3: false,
                    optimistic_v3_bid_adjustment_required: false,
                }),
                priority: Some(0),
                authorization_header: None,
                builder_id_header: None,
                api_token_header: None,
                adjustment_fee_payer: None,
                is_bloxroute: false,
                bloxroute_rproxy_regions: Vec::new(),
                bloxroute_rproxy_only: false,
                ask_for_filtering_validators: None,
                can_ignore_gas_limit: None,
            },
        );
        map.insert(
            "agnostic".to_string(),
            RelayConfig {
                name: "agnostic".to_string(),
                url: "https://0xa7ab7a996c8584251c8f925da3170bdfd6ebc75d50f5ddc4050a6fdc77f2a3b5fce2cc750d0865e05d7228af97d69561@agnostic-relay.net".to_string(),
                grpc_url: None,
                mode: RelayMode::Full,
                submit_config: Some(RelaySubmitConfig {
                    use_ssz_for_submit: true,
                    use_gzip_for_submit: true,
                    optimistic: true,
                    interval_between_submissions_ms: None,
                    max_bid_eth: None,
                    optimistic_v3: false,
                    optimistic_v3_bid_adjustment_required: false,
                }),
                priority: Some(0),
                authorization_header: None,
                builder_id_header: None,
                api_token_header: None,
                adjustment_fee_payer: None,
                is_bloxroute: false,
                bloxroute_rproxy_regions: Vec::new(),
                bloxroute_rproxy_only: false,
                ask_for_filtering_validators: None,
                can_ignore_gas_limit: None,
            },
        );
        map.insert(
            "playground".to_string(),
            RelayConfig {
                name: "playground".to_string(),
                grpc_url: None,
                url: "http://0xac6e77dfe25ecd6110b8e780608cce0dab71fdd5ebea22a16c0205200f2f8e2e3ad3b71d3499c54ad14d6c21b41a37ae@localhost:5555".to_string(),
                mode: RelayMode::Full,
                submit_config: Some(RelaySubmitConfig {
                    use_ssz_for_submit: false,
                    use_gzip_for_submit: false,
                    optimistic: false,
                    interval_between_submissions_ms: None,
                    max_bid_eth: None,
                    optimistic_v3: false,
                    optimistic_v3_bid_adjustment_required: false,
                }),
                priority: Some(0),
                authorization_header: None,
                builder_id_header: None,
                api_token_header: None,
                adjustment_fee_payer: None,
                is_bloxroute: false,
                bloxroute_rproxy_regions: Vec::new(),
                bloxroute_rproxy_only: false,
                ask_for_filtering_validators: None,
                can_ignore_gas_limit: None,
            },
        );
        map
    };
}

pub async fn create_wallet_balance_watcher<P>(
    provider: P,
    base_config: &BaseConfig,
) -> eyre::Result<(WalletBalanceWatcher<P>, Vec<LandedBlockInfo>)>
where
    P: StateProviderFactory + Clone + 'static,
{
    let address = base_config.coinbase_signer()?.address;
    Ok(tokio::task::spawn_blocking(move || {
        WalletBalanceWatcher::new(provider, address, WALLET_INIT_HISTORY_SIZE)
    })
    .await??)
}

pub async fn create_sink_factory_and_relays<P>(
    base_config: &BaseConfig,
    l1_config: &L1Config,
    relay_sets: Vec<RelaySet>,
    wallet_balance_watcher: WalletBalanceWatcher<P>,
    bid_observer: Box<dyn BidObserver + Send + Sync>,
    bidding_service: Arc<dyn BiddingService>,
    cancellation_token: CancellationToken,
) -> eyre::Result<(
    UnfinishedBuiltBlocksInputFactory<P>,
    Vec<MevBoostRelaySlotInfoProvider>,
    ahash::HashMap<MevBoostRelayID, Address>,
)>
where
    P: StateProviderFactory + Clone + 'static,
{
    let (sink_sealed_factory, slot_info_provider, adjustment_fee_payers) = l1_config
        .create_relays_sealed_sink_factory(
            base_config.chain_spec()?,
            relay_sets.clone(),
            bid_observer,
        )?;

    if !l1_config.relay_bid_scrapers.is_empty() {
        let sender = Arc::new(BiddingService2BidSender::new(bidding_service.clone()));
        bid_scraper::bid_scraper::run(
            l1_config.relay_bid_scrapers.clone(),
            sender,
            cancellation_token.clone(),
        );
    }

    let sink_factory = UnfinishedBuiltBlocksInputFactory::new(
        bidding_service,
        sink_sealed_factory,
        wallet_balance_watcher,
        base_config.adjust_finalized_blocks,
        relay_sets,
    );

    Ok((sink_factory, slot_info_provider, adjustment_fee_payers))
}

/// Take the end of the pipeline (sink_factory) + pre-created slot_info_provider and creates an empty builder (it still needs the with_builders to be called)
pub async fn create_builder_from_sink<P>(
    base_config: &BaseConfig,
    l1_config: &L1Config,
    provider: P,
    sink_factory: UnfinishedBuiltBlocksInputFactory<P>,
    slot_info_provider: Vec<MevBoostRelaySlotInfoProvider>,
    adjustment_fee_payers: ahash::HashMap<MevBoostRelayID, Address>,
    cancellation_token: CancellationToken,
) -> eyre::Result<super::LiveBuilder<P>>
where
    P: StateProviderFactory,
{
    let blocklist_provider = base_config
        .blocklist_provider(cancellation_token.clone())
        .await?;

    let payload_event = MevBoostSlotDataGenerator::new(
        l1_config.beacon_clients()?,
        slot_info_provider,
        l1_config.registration_update_interval(),
        adjustment_fee_payers,
        blocklist_provider.clone(),
        cancellation_token.clone(),
    );
    base_config
        .create_builder_with_provider_factory(
            cancellation_token,
            sink_factory,
            payload_event,
            provider,
            blocklist_provider,
        )
        .await
}

#[cfg(test)]
mod test {
    use super::*;
    use alloy_primitives::{address, fixed_bytes};
    use rbuilder_config::load_toml_config;
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
        p.push("../../examples/config/rbuilder/config-live-example.toml");

        let config: Config = load_toml_config(p.clone()).expect("Config load");

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

        let config: Config = load_toml_config(p).expect("Config load");

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

    #[tokio::test]
    async fn test_parse_enabled_relays() {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("./src/live_builder/testdata/config_with_relay_override.toml");

        let config: Config = load_toml_config(p.clone()).expect("Config load");

        let (_, slot_info_providers) = config.l1_config.create_relays().unwrap();
        assert_eq!(slot_info_providers.len(), 1);
        assert_eq!(slot_info_providers[0].id(), "playground");
    }

    #[test]
    fn test_parse_backtest_example_config() {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("../../examples/config/rbuilder/config-backtest-example.toml");

        load_toml_config::<Config>(p).expect("Config load");
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
