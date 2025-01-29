//! Config should always be deserializable, default values should be used
//!
use crate::{
    building::builders::UnfinishedBlockBuildingSinkFactory,
    live_builder::{order_input::OrderInputConfig, LiveBuilder},
    provider::StateProviderFactory,
    roothash::RootHashConfig,
    telemetry::{setup_reloadable_tracing_subscriber, LoggerConfig},
    utils::{
        constants::{MINS_PER_HOUR, SECS_PER_MINUTE},
        http_provider, BoxedProvider, ProviderFactoryReopener, Signer,
    },
};
use alloy_primitives::{Address, B256};
use eyre::{eyre, Context};
use jsonrpsee::RpcModule;
use reth::chainspec::chain_value_parser;
use reth_chainspec::ChainSpec;
use reth_db::DatabaseEnv;
use reth_node_api::NodeTypesWithDBAdapter;
use reth_node_ethereum::EthereumNode;
use reth_primitives::StaticFileSegment;
use reth_provider::StaticFileProviderFactory;
use serde::{Deserialize, Deserializer};
use serde_with::{serde_as, DeserializeAs};
use std::{
    env::var,
    fs::read_to_string,
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
    SlotSource,
};

/// Prefix for env variables in config
const ENV_PREFIX: &str = "env:";

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
    /// Enables dynamic logging (saving logs to a file)
    pub log_enable_dynamic: bool,

    pub error_storage_path: Option<PathBuf>,

    coinbase_secret_key: Option<EnvOrValue<String>>,

    pub flashbots_db: Option<EnvOrValue<String>>,

    pub el_node_ipc_path: Option<PathBuf>,
    pub jsonrpc_server_port: u16,
    #[serde(default = "default_ip")]
    pub jsonrpc_server_ip: Ipv4Addr,

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

    #[serde(deserialize_with = "deserialize_extra_data")]
    pub extra_data: Vec<u8>,

    /// mev-share bundles coming from this address are treated in a special way(see [`ShareBundleMerger`])
    pub sbundle_mergeable_signers: Option<Vec<Address>>,

    /// Backwards compatible typo soon to be removed.
    pub sbundle_mergeabe_signers: Option<Vec<Address>>,

    /// Number of threads used for incoming order simulation
    pub simulation_threads: usize,

    /// uses cached sparse trie for root hash
    pub root_hash_use_sparse_trie: bool,
    /// compares result of root hash using sparse trie and reference root hash
    pub root_hash_compare_sparse_trie: bool,

    pub watchdog_timeout_sec: Option<u64>,

    /// List of `builders` to be used for live building
    pub live_builders: Vec<String>,

    // backtest config
    backtest_fetch_mempool_data_dir: EnvOrValue<String>,
    pub backtest_fetch_eth_rpc_url: String,
    pub backtest_fetch_eth_rpc_parallel: usize,
    pub backtest_fetch_output_file: PathBuf,
    /// List of `builders` to be used in backtest run
    pub backtest_builders: Vec<String>,
    pub backtest_results_store_path: PathBuf,
    pub backtest_protect_bundle_signers: Vec<Address>,
}

pub fn default_ip() -> Ipv4Addr {
    Ipv4Addr::new(0, 0, 0, 0)
}

/// Loads config from toml file, some values can be loaded from env variables with the following syntax
/// e.g. flashbots_db = "env:FLASHBOTS_DB"
///
/// variables that can be configured with env values:
/// - log_level
/// - coinbase_secret_key
/// - flashbots_db
/// - relay_secret_key
/// - optimistic_relay_secret_key
/// - backtest_fetch_mempool_data_dir
pub fn load_config_toml_and_env<T: serde::de::DeserializeOwned>(
    path: impl AsRef<Path>,
) -> eyre::Result<T> {
    let data = read_to_string(path.as_ref()).with_context(|| {
        eyre!(
            "Config file read error: {:?}",
            path.as_ref().to_string_lossy()
        )
    })?;

    let config: T = toml::from_str(&data).context("Config file parsing")?;
    Ok(config)
}

impl BaseConfig {
    pub fn setup_tracing_subscriber(&self) -> eyre::Result<()> {
        let log_level = self.log_level.value()?;
        let config = LoggerConfig {
            env_filter: log_level,
            file: None,
            log_json: self.log_json,
            log_color: self.log_color,
        };
        setup_reloadable_tracing_subscriber(config)?;
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

    /// Allows instantiating a [`LiveBuilder`] with an existing provider factory
    pub async fn create_builder_with_provider_factory<P, SlotSourceType>(
        &self,
        cancellation_token: tokio_util::sync::CancellationToken,
        sink_factory: Box<dyn UnfinishedBlockBuildingSinkFactory>,
        slot_source: SlotSourceType,
        provider: P,
        blocklist_provider: Arc<dyn BlockListProvider>,
    ) -> eyre::Result<super::LiveBuilder<P, SlotSourceType>>
    where
        P: StateProviderFactory,
        SlotSourceType: SlotSource,
    {
        let order_input_config = OrderInputConfig::from_config(self)?;
        let (orderpool_sender, orderpool_receiver) =
            mpsc::channel(order_input_config.input_channel_buffer_size);
        Ok(LiveBuilder::<P, SlotSourceType> {
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

            global_cancellation: cancellation_token,

            extra_rpc: RpcModule::new(()),
            sink_factory,
            builders: Vec::new(),

            run_sparse_trie_prefetcher: self.root_hash_use_sparse_trie,

            orderpool_sender,
            orderpool_receiver,
            sbundle_merger_selected_signers: Arc::new(self.sbundle_mergeable_signers()),
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
    pub fn create_provider_factory(
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

    pub fn live_root_hash_config(&self) -> eyre::Result<RootHashConfig> {
        if self.root_hash_compare_sparse_trie && !self.root_hash_use_sparse_trie {
            eyre::bail!(
                "root_hash_compare_sparse_trie can't be set without root_hash_use_sparse_trie"
            );
        }
        Ok(RootHashConfig::new(
            self.root_hash_use_sparse_trie,
            self.root_hash_compare_sparse_trie,
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
        validate_blocklist: bool,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> eyre::Result<Arc<dyn BlockListProvider>> {
        if self.blocklist.is_some() && self.blocklist_file_path.is_some() {
            eyre::bail!("You can't use blocklist AND blocklist_file_path")
        }

        if let Some(blocklist) = &self.blocklist {
            // First try url loading
            match Url::parse(blocklist) {
                Ok(url) => {
                    return self
                        .blocklist_provider_from_url(url, validate_blocklist, cancellation_token)
                        .await;
                }
                Err(_) => {
                    // second try file loading
                    return self
                        .blocklist_provider_from_file(&blocklist.into(), validate_blocklist);
                }
            }
        }

        // Backwards compatibility
        if let Some(blocklist_file_path) = &self.blocklist_file_path {
            warn!("blocklist_file_path is deprecated please use blocklist");
            return self.blocklist_provider_from_file(blocklist_file_path, validate_blocklist);
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
        let max_allowed_age_hours = self
            .blocklist_url_max_age_hours
            .unwrap_or(DEFAULT_BLOCKLIST_URL_MAX_AGE_HOURS);
        let max_allowed_age =
            Duration::from_secs(max_allowed_age_hours * SECS_PER_MINUTE * MINS_PER_HOUR);
        let provider = HttpBlockListProvider::new(
            blocklist_url,
            max_allowed_age,
            validate_blocklist,
            cancellation_token,
        )
        .await?;
        Ok(Arc::new(provider))
    }

    pub fn eth_rpc_provider(&self) -> eyre::Result<BoxedProvider> {
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvOrValue<T>(String, std::marker::PhantomData<T>);

impl<T: FromStr> EnvOrValue<T> {
    pub fn value(&self) -> eyre::Result<String> {
        let value = &self.0;
        if value.starts_with(ENV_PREFIX) {
            let var_name = value.trim_start_matches(ENV_PREFIX);
            var(var_name).map_err(|_| eyre::eyre!("Env variable: {} not set", var_name))
        } else {
            Ok(value.to_string())
        }
    }
}

impl<T> From<&str> for EnvOrValue<T> {
    fn from(s: &str) -> Self {
        Self(s.to_string(), std::marker::PhantomData)
    }
}

impl<'de, T: FromStr> Deserialize<'de> for EnvOrValue<T> {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self(s, std::marker::PhantomData))
    }
}

// Helper function to resolve Vec<EnvOrValue<T>> to Vec<T>
pub fn resolve_env_or_values<T: FromStr>(values: &[EnvOrValue<T>]) -> eyre::Result<Vec<T>> {
    values
        .iter()
        .try_fold(Vec::new(), |mut acc, v| -> eyre::Result<Vec<T>> {
            let value = v.value()?;
            if v.0.starts_with(ENV_PREFIX) {
                // If it's an environment variable, split by comma
                let parsed: eyre::Result<Vec<T>> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        T::from_str(s).map_err(|_| eyre::eyre!("Failed to parse value: {}", s))
                    })
                    .collect();
                acc.extend(parsed?);
            } else {
                // If it's not an environment variable, just return the single value
                acc.push(
                    T::from_str(&value)
                        .map_err(|_| eyre::eyre!("Failed to parse value: {}", value))?,
                );
            }
            Ok(acc)
        })
}

impl<'de, T> DeserializeAs<'de, EnvOrValue<T>> for EnvOrValue<T>
where
    T: FromStr,
    String: Deserialize<'de>,
{
    fn deserialize_as<D>(deserializer: D) -> Result<EnvOrValue<T>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(EnvOrValue(s, std::marker::PhantomData))
    }
}

pub const DEFAULT_CL_NODE_URL: &str = "http://127.0.0.1:3500";
pub const DEFAULT_EL_NODE_IPC_PATH: &str = "/tmp/reth.ipc";
pub const DEFAULT_INCOMING_BUNDLES_PORT: u16 = 8645;
pub const DEFAULT_RETH_DB_PATH: &str = "/mnt/data/reth";
/// This will update every 2.4 hours, super reasonable.
pub const DEFAULT_BLOCKLIST_URL_MAX_AGE_HOURS: u64 = 24;

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
            log_enable_dynamic: false,
            error_storage_path: None,
            coinbase_secret_key: None,
            flashbots_db: None,
            el_node_ipc_path: None,
            jsonrpc_server_port: DEFAULT_INCOMING_BUNDLES_PORT,
            jsonrpc_server_ip: default_ip(),
            ignore_cancellable_orders: true,
            ignore_blobs: false,
            chain: "mainnet".to_string(),
            reth_datadir: Some(DEFAULT_RETH_DB_PATH.parse().unwrap()),
            reth_db_path: None,
            reth_static_files_path: None,
            blocklist_file_path: None,
            blocklist: None,
            blocklist_url_max_age_hours: None,
            extra_data: b"extra_data_change_me".to_vec(),
            root_hash_use_sparse_trie: false,
            root_hash_compare_sparse_trie: false,
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
            sbundle_mergeable_signers: None,
            sbundle_mergeabe_signers: None,
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
    root_hash_config: Option<RootHashConfig>,
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

    #[test]
    fn test_default_config() {
        let config: BaseConfig = serde_json::from_str("{}").unwrap();
        let config_default = BaseConfig::default();

        assert_eq!(config, config_default);
    }

    #[test]
    fn test_reth_db() {
        // Setup and initialize a temp reth db (with static files)
        let tempdir = TempDir::with_prefix_in("rbuilder-", "/tmp").unwrap();

        let data_dir = MaybePlatformPath::<DataDirPath>::from(tempdir.into_path());
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
