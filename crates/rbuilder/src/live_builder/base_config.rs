//! Config should always be deserializable, default values should be used
//!
use crate::{
    live_builder::{
        order_flow_tracing::order_flow_tracer_manager::{
            NullOrderFlowTracerManager, OrderFlowTracerManager, OrderFlowTracerManagerImpl,
        },
        order_input::OrderInputConfig,
        process_killer::ProcessKiller,
        LiveBuilder,
    },
    provider::{
        ipc_state_provider::{IpcProviderConfig, IpcStateProviderFactory},
        StateProviderFactory,
    },
    roothash::RootHashContext,
    utils::{
        constants::{MINS_PER_HOUR, SECS_PER_MINUTE},
        http_provider, ProviderFactoryReopener, Signer,
    },
};
use alloy_primitives::{Address, B256};
use alloy_provider::RootProvider;
use eth_sparse_mpt::{ETHSpareMPTVersion, RootHashThreadPool};
use eyre::Context;
use jsonrpsee::RpcModule;
use rbuilder_config::{EnvOrValue, LoggerConfig};
use reth::chainspec::chain_value_parser;
use reth_chainspec::ChainSpec;
use reth_db::DatabaseEnv;
use reth_node_api::NodeTypesWithDBAdapter;
use reth_node_ethereum::EthereumNode;
use reth_primitives::StaticFileSegment;
use reth_provider::StaticFileProviderFactory;
use serde::{Deserialize, Deserializer};
use serde_with::serde_as;
use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;
use tracing::{error, warn};
use url::Url;

use super::{
    block_list_provider::{
        BlockListProvider, HttpBlockListProvider, NullBlockListProvider,
        StaticFileBlockListProvider,
    },
    block_output::unfinished_block_processing::UnfinishedBuiltBlocksInputFactory,
    payload_events::MevBoostSlotDataGenerator,
};

/// Base config to be used by all builders.
/// It allows us to create a base LiveBuilder with no algorithms or custom bidding.
/// The final configuration should usually include one of this and use it to create the base LiveBuilder to then upgrade it as needed.
#[serde_as]
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct BaseConfig {
    pub full_telemetry_server_port: u16,
    #[serde(default = "default_ip")]
    pub full_telemetry_server_ip: Ipv4Addr,

    pub redacted_telemetry_server_port: u16,
    #[serde(default = "default_ip")]
    pub redacted_telemetry_server_ip: Ipv4Addr,

    pub log_json: bool,
    log_level: EnvOrValue<String>,
    pub log_color: bool,

    pub error_storage_path: Option<PathBuf>,

    coinbase_secret_key: Option<EnvOrValue<String>>,

    pub el_node_ipc_path: Option<PathBuf>,
    pub jsonrpc_server_port: u16,
    #[serde(default = "default_ip")]
    pub jsonrpc_server_ip: Ipv4Addr,
    pub jsonrpc_server_max_connections: Option<u32>,

    pub ignore_cancellable_orders: bool,
    pub ignore_blobs: bool,

    pub chain: String,
    pub reth_datadir: Option<PathBuf>,
    pub reth_db_path: Option<PathBuf>,
    pub reth_static_files_path: Option<PathBuf>,

    /// Backwards compatibility. Downloads blocklist from a file.
    /// Same as setting a file name on blocklist.
    pub blocklist_file_path: Option<PathBuf>,

    /// Can contain an url or a file name.
    /// If it's a url download blocklist from url and updates periodically.
    /// If it's a filename just loads the file (no updates).
    pub blocklist: Option<String>,

    /// If the downloaded file get older than this we abort.
    pub blocklist_url_max_age_hours: Option<u64>,

    /// Like blocklist_url_max_age_hours but in secs for integration tests.
    pub blocklist_url_max_age_secs: Option<u64>,

    /// if true will not allow to start without a blocklist or with an empty blocklist.
    pub require_non_empty_blocklist: Option<bool>,

    #[serde(deserialize_with = "deserialize_extra_data")]
    pub extra_data: Vec<u8>,

    /// mev-share bundles coming from this address are treated in a special way(see [`ShareBundleMerger`])
    pub sbundle_mergeable_signers: Option<Vec<Address>>,

    /// Backwards compatible typo soon to be removed.
    pub sbundle_mergeabe_signers: Option<Vec<Address>>,

    /// Number of threads used for incoming order simulation
    pub simulation_threads: usize,
    pub simulation_use_random_coinbase: bool,

    /// uses cached sparse trie for root hash
    pub root_hash_use_sparse_trie: bool,
    /// uses cached sparse trie for root hash
    pub root_hash_sparse_trie_version: String,
    /// compares result of root hash using sparse trie and reference root hash
    pub root_hash_compare_sparse_trie: bool,
    /// number of threads used for root hash thread pool
    /// if 0 global rayon pool is used
    root_hash_threads: usize,

    /// use pipelined finalization where blocks are "prefinalized" first
    /// and payment tx is inserted later for faster bidding response time
    pub adjust_finalized_blocks: bool,

    pub watchdog_timeout_sec: Option<u64>,

    /// List of `builders` to be used for live building
    pub live_builders: Vec<String>,

    /// See [BlockBuildingHelperFromProvider::max_order_execution_duration_warning]
    pub max_order_execution_duration_warning_us: Option<u64>,

    /// Config for IPC state provider
    pub ipc_provider: Option<IpcProviderConfig>,

    pub evm_caching_enable: bool,
    /// Use experimental code for faster finalize
    pub faster_finalize: bool,

    /// See [OrderPool::time_to_keep_mempool_txs]
    pub time_to_keep_mempool_txs_secs: u64,

    /// The array of senders incoming transactions from which will not be counted towards the coinbase profit.
    pub system_recipient_allowlist: Vec<Address>,

    // backtest config
    backtest_fetch_mempool_data_dir: EnvOrValue<String>,
    pub backtest_fetch_eth_rpc_url: String,
    pub backtest_fetch_eth_rpc_parallel: usize,
    pub backtest_fetch_output_file: PathBuf,
    /// List of `builders` to be used in backtest run
    pub backtest_builders: Vec<String>,
    pub backtest_results_store_path: PathBuf,
    pub backtest_protect_bundle_signers: Vec<Address>,

    /// We will store a file per block in this path.
    pub orderflow_tracing_store_path: Option<PathBuf>,
    /// Max number of blocks to keep in disk.
    pub orderflow_tracing_max_blocks: usize,
}

pub fn default_ip() -> Ipv4Addr {
    Ipv4Addr::new(0, 0, 0, 0)
}

impl BaseConfig {
    pub fn setup_tracing_subscriber(&self) -> eyre::Result<()> {
        let log_level = self.log_level.value()?;
        let config = LoggerConfig {
            env_filter: log_level,
            log_json: self.log_json,
            log_color: self.log_color,
        };
        config.init_tracing()?;
        Ok(())
    }

    pub fn redacted_telemetry_server_address(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(
            self.redacted_telemetry_server_ip,
            self.redacted_telemetry_server_port,
        ))
    }

    pub fn full_telemetry_server_address(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(
            self.full_telemetry_server_ip,
            self.full_telemetry_server_port,
        ))
    }

    pub fn root_hash_thread_pool(&self) -> eyre::Result<Option<RootHashThreadPool>> {
        let root_hash_thread_pool = if self.root_hash_threads > 0 {
            Some(RootHashThreadPool::try_new(self.root_hash_threads)?)
        } else {
            None
        };
        Ok(root_hash_thread_pool)
    }

    /// Allows instantiating a [`LiveBuilder`] with an existing provider factory
    pub async fn create_builder_with_provider_factory<P>(
        &self,
        cancellation_token: tokio_util::sync::CancellationToken,
        unfinished_built_blocks_input_factory: UnfinishedBuiltBlocksInputFactory<P>,
        slot_source: MevBoostSlotDataGenerator,
        provider: P,
        blocklist_provider: Arc<dyn BlockListProvider>,
    ) -> eyre::Result<super::LiveBuilder<P>>
    where
        P: StateProviderFactory,
    {
        let order_input_config = OrderInputConfig::from_config(self)?;
        let (orderpool_sender, orderpool_receiver) =
            mpsc::channel(order_input_config.input_channel_buffer_size);

        let order_flow_tracer_manager: Box<dyn OrderFlowTracerManager> =
            if let Some(orderflow_tracing_store_path) = &self.orderflow_tracing_store_path {
                if self.orderflow_tracing_max_blocks != 0 {
                    Box::new(OrderFlowTracerManagerImpl::new(
                        orderflow_tracing_store_path.clone(),
                        self.orderflow_tracing_max_blocks,
                    )?)
                } else {
                    Box::new(NullOrderFlowTracerManager {})
                }
            } else {
                Box::new(NullOrderFlowTracerManager {})
            };

        Ok(LiveBuilder::<P> {
            watchdog_timeout: self.watchdog_timeout(),
            error_storage_path: self.error_storage_path.clone(),
            simulation_threads: self.simulation_threads,
            order_input_config,
            blocks_source: slot_source,
            chain_chain_spec: self.chain_spec()?,
            provider,

            coinbase_signer: self.coinbase_signer()?,
            extra_data: self.extra_data.clone(),
            blocklist_provider,

            global_cancellation: cancellation_token.clone(),
            process_killer: ProcessKiller::new(cancellation_token),
            extra_rpc: RpcModule::new(()),
            unfinished_built_blocks_input_factory,
            builders: Vec::new(),

            run_sparse_trie_prefetcher: self.root_hash_use_sparse_trie,

            orderpool_sender,
            orderpool_receiver,
            sbundle_merger_selected_signers: Arc::new(self.sbundle_mergeable_signers()),

            evm_caching_enable: self.evm_caching_enable,
            simulation_use_random_coinbase: self.simulation_use_random_coinbase,
            faster_finalize: self.faster_finalize,
            order_flow_tracer_manager,
        })
    }

    pub fn chain_spec(&self) -> eyre::Result<Arc<ChainSpec>> {
        chain_value_parser(&self.chain)
    }

    pub fn sbundle_mergeable_signers(&self) -> Vec<Address> {
        if let Some(sbundle_mergeable_signers) = &self.sbundle_mergeable_signers {
            if self.sbundle_mergeabe_signers.is_some() {
                error!("sbundle_mergeable_signers and sbundle_mergeabe_signers found. Will use bundle_mergeable_signers");
            }
            sbundle_mergeable_signers.clone()
        } else if let Some(sbundle_mergeable_signers) = &self.sbundle_mergeabe_signers {
            warn!("sbundle_mergeable_signers missing but found sbundle_mergeabe_signers. sbundle_mergeabe_signers will be used but this will be deprecated soon");
            sbundle_mergeable_signers.clone()
        } else {
            warn!("Defaulting sbundle_mergeable_signers to empty. We may not comply with order flow rules.");
            Vec::default()
        }
    }

    /// Open reth db and DB should be opened once per process but it can be cloned and moved to different threads.
    /// skip_root_hash -> will create a mock roothasher. Used on backtesting since reth can't compute roothashes on the past.
    pub fn create_reth_provider_factory(
        &self,
        skip_root_hash: bool,
    ) -> eyre::Result<ProviderFactoryReopener<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>>
    {
        create_provider_factory(
            self.reth_datadir.as_deref(),
            self.reth_db_path.as_deref(),
            self.reth_static_files_path.as_deref(),
            self.chain_spec()?,
            false,
            if skip_root_hash {
                None
            } else {
                Some(self.live_root_hash_config()?)
            },
        )
    }

    /// Opens IPC connection to node that will provide the sate
    pub fn create_ipc_provider_factory(&self) -> eyre::Result<IpcStateProviderFactory> {
        let ipc_provider_config = self
            .ipc_provider
            .as_ref()
            .ok_or_else(|| eyre::eyre!("IPC provider not configured"))?;

        Ok(IpcStateProviderFactory::new(
            &ipc_provider_config.ipc_path,
            Duration::from_millis(ipc_provider_config.request_timeout_ms),
        ))
    }

    /// live_root_hash_config creates a root hash thread pool
    /// so it should be called once on the startup and cloned if needed
    pub fn live_root_hash_config(&self) -> eyre::Result<RootHashContext> {
        if self.root_hash_compare_sparse_trie && !self.root_hash_use_sparse_trie {
            eyre::bail!(
                "root_hash_compare_sparse_trie can't be set without root_hash_use_sparse_trie"
            );
        }
        // temporary guard until reth is fixed
        if !self.root_hash_use_sparse_trie || self.root_hash_compare_sparse_trie {
            eyre::bail!("root_hash_use_sparse_trie=true and root_hash_compare_sparse_trie=false must be set, otherwise node will produce incorrect blocks or confusing error messages. These settings are enforced temporarily because upstream parallel root hash implementation is not correct.")
        }
        let thread_pool = self.root_hash_thread_pool()?;
        let version = match self.root_hash_sparse_trie_version.as_str() {
            "v1" => ETHSpareMPTVersion::V1,
            "v2" => ETHSpareMPTVersion::V2,
            _ => eyre::bail!("root_hash_sparse_trie_version can be v1 or v2"),
        };
        Ok(RootHashContext::new(
            self.root_hash_use_sparse_trie,
            self.root_hash_compare_sparse_trie,
            thread_pool,
            version,
        ))
    }

    pub fn coinbase_signer(&self) -> eyre::Result<Signer> {
        if let Some(secret_key) = &self.coinbase_secret_key {
            return coinbase_signer_from_secret_key(&secret_key.value()?);
        }
        warn!("No coinbase secret key provided. A random key will be generated.");
        warn!(
            "Caution: If this node wins any block, you wont be able to access the rewards for it."
        );
        let new_signer = Signer::random();
        Ok(new_signer)
    }

    pub async fn blocklist_provider(
        &self,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> eyre::Result<Arc<dyn BlockListProvider>> {
        if self.blocklist.is_some() && self.blocklist_file_path.is_some() {
            eyre::bail!("You can't use blocklist AND blocklist_file_path")
        }

        let require_non_empty_blocklist = self
            .require_non_empty_blocklist
            .unwrap_or(DEFAULT_REQUIRE_NON_EMPTY_BLOCKLIST);
        if self.blocklist_file_path.is_none()
            && self.blocklist.is_none()
            && require_non_empty_blocklist
        {
            eyre::bail!("require_non_empty_blocklist = true but no blocklist used (blocklist_file_path/blocklist are not set)");
        }

        if let Some(blocklist) = &self.blocklist {
            // First try url loading
            match Url::parse(blocklist) {
                Ok(url) => {
                    return self
                        .blocklist_provider_from_url(
                            url,
                            require_non_empty_blocklist,
                            cancellation_token,
                        )
                        .await;
                }
                Err(_) => {
                    // second try file loading
                    return self.blocklist_provider_from_file(
                        &blocklist.into(),
                        require_non_empty_blocklist,
                    );
                }
            }
        }

        // Backwards compatibility
        if let Some(blocklist_file_path) = &self.blocklist_file_path {
            warn!("blocklist_file_path is deprecated please use blocklist");
            return self
                .blocklist_provider_from_file(blocklist_file_path, require_non_empty_blocklist);
        }

        // default to empty
        Ok(Arc::new(NullBlockListProvider::new()))
    }

    pub fn blocklist_provider_from_file(
        &self,
        blocklist_file_path: &PathBuf,
        validate_blocklist: bool,
    ) -> eyre::Result<Arc<dyn BlockListProvider>> {
        Ok(Arc::new(StaticFileBlockListProvider::new(
            blocklist_file_path,
            validate_blocklist,
        )?))
    }

    pub async fn blocklist_provider_from_url(
        &self,
        blocklist_url: Url,
        validate_blocklist: bool,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> eyre::Result<Arc<dyn BlockListProvider>> {
        let max_allowed_age_secs =
            if let Some(max_allowed_age_hours) = self.blocklist_url_max_age_hours {
                max_allowed_age_hours * SECS_PER_MINUTE * MINS_PER_HOUR
            } else if let Some(blocklist_url_max_age_secs) = self.blocklist_url_max_age_secs {
                blocklist_url_max_age_secs
            } else {
                DEFAULT_BLOCKLIST_URL_MAX_AGE_HOURS * SECS_PER_MINUTE * MINS_PER_HOUR
            };
        let max_allowed_age = Duration::from_secs(max_allowed_age_secs);
        let provider = HttpBlockListProvider::new(
            blocklist_url,
            max_allowed_age,
            validate_blocklist,
            cancellation_token,
        )
        .await?;
        Ok(Arc::new(provider))
    }

    pub fn eth_rpc_provider(&self) -> eyre::Result<RootProvider> {
        Ok(http_provider(self.backtest_fetch_eth_rpc_url.parse()?))
    }

    pub fn watchdog_timeout(&self) -> Option<Duration> {
        match self.watchdog_timeout_sec {
            Some(0) => None,
            Some(sec) => Some(Duration::from_secs(sec)),
            None => None,
        }
    }

    pub fn backtest_fetch_mempool_data_dir(&self) -> eyre::Result<PathBuf> {
        let path = self.backtest_fetch_mempool_data_dir.value()?;
        let path_expanded = shellexpand::tilde(&path).to_string();

        Ok(path_expanded.parse()?)
    }
    pub fn max_order_execution_duration_warning(&self) -> Option<Duration> {
        self.max_order_execution_duration_warning_us
            .map(Duration::from_micros)
    }
}

pub const DEFAULT_CL_NODE_URL: &str = "http://127.0.0.1:3500";
pub const DEFAULT_EL_NODE_IPC_PATH: &str = "/tmp/reth.ipc";
pub const DEFAULT_INCOMING_BUNDLES_PORT: u16 = 8645;
pub const DEFAULT_RETH_DB_PATH: &str = "/mnt/data/reth";
/// This will update every 2.4 hours, super reasonable.
pub const DEFAULT_BLOCKLIST_URL_MAX_AGE_HOURS: u64 = 24;
pub const DEFAULT_REQUIRE_NON_EMPTY_BLOCKLIST: bool = false;
pub const DEFAULT_TIME_TO_KEEP_MEMPOOL_TXS_SECS: u64 = 60;

impl Default for BaseConfig {
    fn default() -> Self {
        Self {
            full_telemetry_server_port: 6069,
            full_telemetry_server_ip: default_ip(),
            redacted_telemetry_server_port: 6070,
            redacted_telemetry_server_ip: default_ip(),
            log_json: false,
            log_level: "info".into(),
            log_color: false,
            error_storage_path: None,
            coinbase_secret_key: None,
            el_node_ipc_path: None,
            jsonrpc_server_port: DEFAULT_INCOMING_BUNDLES_PORT,
            jsonrpc_server_ip: default_ip(),
            jsonrpc_server_max_connections: None,
            ignore_cancellable_orders: true,
            ignore_blobs: false,
            chain: "mainnet".to_string(),
            reth_datadir: Some(DEFAULT_RETH_DB_PATH.parse().unwrap()),
            reth_db_path: None,
            reth_static_files_path: None,
            blocklist_file_path: None,
            blocklist: None,
            blocklist_url_max_age_hours: None,
            blocklist_url_max_age_secs: None,
            extra_data: b"extra_data_change_me".to_vec(),
            root_hash_use_sparse_trie: false,
            root_hash_sparse_trie_version: "v1".to_string(),
            root_hash_compare_sparse_trie: false,
            root_hash_threads: 0,
            adjust_finalized_blocks: false,
            watchdog_timeout_sec: None,
            backtest_fetch_mempool_data_dir: "/mnt/data/mempool".into(),
            backtest_fetch_eth_rpc_url: "http://127.0.0.1:8545".to_string(),
            backtest_fetch_eth_rpc_parallel: 1,
            backtest_fetch_output_file: "/tmp/rbuilder-backtest.sqlite".parse().unwrap(),
            backtest_results_store_path: "/tmp/rbuilder-backtest-results.sqlite".parse().unwrap(),
            backtest_protect_bundle_signers: vec![],
            backtest_builders: Vec::new(),
            live_builders: vec!["mgp-ordering".to_string(), "mp-ordering".to_string()],
            simulation_threads: 1,
            simulation_use_random_coinbase: true,
            sbundle_mergeable_signers: None,
            sbundle_mergeabe_signers: None,
            require_non_empty_blocklist: Some(DEFAULT_REQUIRE_NON_EMPTY_BLOCKLIST),
            ipc_provider: None,
            evm_caching_enable: false,
            faster_finalize: false,
            time_to_keep_mempool_txs_secs: DEFAULT_TIME_TO_KEEP_MEMPOOL_TXS_SECS,
            orderflow_tracing_store_path: None,
            orderflow_tracing_max_blocks: 0,
            system_recipient_allowlist: Vec::new(),
            max_order_execution_duration_warning_us: None,
        }
    }
}

fn deserialize_extra_data<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    let bytes = s.into_bytes();
    if bytes.len() > 32 {
        return Err(serde::de::Error::custom(
            "Extra data is too long (max 32 bytes)",
        ));
    }
    Ok(bytes)
}

/// Open reth db and DB should be opened once per process but it can be cloned and moved to different threads.
/// root_hash_config None -> MockRootHasher used
pub fn create_provider_factory(
    reth_datadir: Option<&Path>,
    reth_db_path: Option<&Path>,
    reth_static_files_path: Option<&Path>,
    chain_spec: Arc<ChainSpec>,
    rw: bool,
    root_hash_config: Option<RootHashContext>,
) -> eyre::Result<ProviderFactoryReopener<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>> {
    // shellexpand the reth datadir
    let reth_datadir = if let Some(reth_datadir) = reth_datadir {
        let reth_datadir = reth_datadir
            .to_str()
            .ok_or_else(|| eyre::eyre!("Invalid UTF-8 in path"))?;

        Some(PathBuf::from(shellexpand::full(reth_datadir)?.into_owned()))
    } else {
        None
    };

    let reth_db_path = match (reth_db_path, reth_datadir.clone()) {
        (Some(reth_db_path), _) => PathBuf::from(reth_db_path),
        (None, Some(reth_datadir)) => reth_datadir.join("db"),
        (None, None) => eyre::bail!("Either reth_db_path or reth_datadir must be provided"),
    };

    let db = if rw {
        open_reth_db_rw(&reth_db_path)
    } else {
        open_reth_db(&reth_db_path)
    }?;

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

fn open_reth_db_rw(reth_db_path: &Path) -> eyre::Result<Arc<DatabaseEnv>> {
    Ok(Arc::new(
        reth_db::open_db(reth_db_path, Default::default()).context("DB open error")?,
    ))
}

pub fn coinbase_signer_from_secret_key(secret_key: &str) -> eyre::Result<Signer> {
    let secret_key = B256::from_str(secret_key)?;
    Ok(Signer::try_from_secret(secret_key)?)
}

#[cfg(test)]
mod test {
    use super::*;
    use reth::args::DatadirArgs;
    use reth_chainspec::{Chain, SEPOLIA};
    use reth_db::init_db;
    use reth_db_common::init::init_genesis;
    use reth_node_core::dirs::{DataDirPath, MaybePlatformPath};
    use reth_provider::{providers::StaticFileProvider, ProviderFactory};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn test_default_config() {
        let config: BaseConfig = serde_json::from_str("{}").unwrap();
        let config_default = BaseConfig::default();

        assert_eq!(config, config_default);
    }

    #[tokio::test]
    async fn test_require_non_empty_blocklist() {
        let config = BaseConfig {
            blocklist: None,
            blocklist_file_path: None,
            require_non_empty_blocklist: Some(true),
            ..Default::default()
        };
        assert!(config
            .blocklist_provider(CancellationToken::new())
            .await
            .is_err());
    }

    #[test]
    fn test_reth_db() {
        // Setup and initialize a temp reth db (with static files)
        let tempdir = TempDir::with_prefix_in("rbuilder-", "/tmp").unwrap();

        let data_dir = MaybePlatformPath::<DataDirPath>::from(tempdir.keep());
        let data_dir = data_dir.unwrap_or_chain_default(Chain::mainnet(), DatadirArgs::default());

        let db = Arc::new(init_db(data_dir.data_dir(), Default::default()).unwrap());
        let provider_factory = ProviderFactory::<NodeTypesWithDBAdapter<EthereumNode, _>>::new(
            db,
            SEPOLIA.clone(),
            StaticFileProvider::read_write(data_dir.static_files().as_path()).unwrap(),
        );
        init_genesis(&provider_factory).unwrap();

        // Create longer-lived PathBuf values
        let data_dir_path = data_dir.data_dir();
        let db_path = data_dir.db();
        let static_files_path = data_dir.static_files();

        let test_cases = [
            // use main dir to resolve reth_db and static_files
            (Some(data_dir_path), None, None, true),
            // use main dir to resolve reth_db and provide static_files
            (
                Some(data_dir_path),
                None,
                Some(static_files_path.clone()),
                true,
            ),
            // provide both reth_db and static_files
            (
                None,
                Some(db_path.as_path()),
                Some(static_files_path.clone()),
                true,
            ),
            // fail to provide main dir to resolve empty static_files
            (None, Some(db_path.as_path()), None, false),
            // fail to provide main dir to resolve empty reth_db
            (None, None, Some(static_files_path), false),
        ];

        for (reth_datadir_path, reth_db_path, reth_static_files_path, should_succeed) in
            test_cases.iter()
        {
            let result = create_provider_factory(
                reth_datadir_path.as_deref(),
                reth_db_path.as_deref(),
                reth_static_files_path.as_deref(),
                Default::default(),
                true,
                None,
            );

            if *should_succeed {
                assert!(
                    result.is_ok(),
                    "Expected success, but got error: {:?}",
                    result.err()
                );
            } else {
                assert!(result.is_err(), "Expected error, but got success");
            }
        }
    }
}
