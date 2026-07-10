use crate::{
    building::{builders::mock_block_building_helper::MockRootHasher, ThreadBlockBuildingContext},
    live_builder::simulation::SimulatedOrderCommand,
    provider::{RootHasher, StateProviderFactory},
    roothash::{
        calculate_account_proofs, calculate_state_root, run_trie_prefetcher, RootHashContext,
        RootHashError,
    },
    telemetry::{inc_provider_bad_reopen_counter, inc_provider_reopen_counter},
};
use alloy_consensus::Header;
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, BlockHash, BlockNumber, Bytes, B256};
use eth_sparse_mpt::*;
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _,
};
use parking_lot::Mutex;
use rbuilder_utils::reth_db::open_rocksdb_read_only;
use reth::providers::{BlockHashReader, ChainSpecProvider, ProviderFactory};
use reth_db::DatabaseError;
use reth_errors::{ProviderError, ProviderResult, RethResult};
use reth_node_api::{NodePrimitives, NodeTypesWithDB};
use reth_provider::{
    providers::{ProviderNodeTypes, StaticFileProvider},
    BlockNumReader, BlockReader, ChangeSetReader, DBProvider, DatabaseProviderFactory,
    HashedPostStateProvider, HeaderProvider, PruneCheckpointReader, StageCheckpointReader,
    StateProviderBox, StorageChangeSetReader, StorageSettingsCache,
};
use revm::database::BundleState;
use std::{
    ops::DerefMut,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
};
use tracing::{debug, error, warn};

/// On-disk paths a [`ProviderFactoryReopener`] needs to recreate its [`ProviderFactory`] when it
/// detects an inconsistency. `None` for factories built via [`ProviderFactoryReopener::new_from_existing`]
/// (tests), which cannot recover these paths from an existing factory and never reopen.
#[derive(Debug, Clone)]
struct ReopenPaths {
    static_files_path: PathBuf,
    rocksdb_path: PathBuf,
}

/// This struct is used as a workaround for https://github.com/paradigmxyz/reth/issues/7836
/// it shares one instance of the provider factory that is recreated when inconsistency is detected.
/// This struct should be used on the level of the whole program and ProviderFactory should be extracted from it
/// into the methods that has a lifetime of a slot (e.g. building particular block).
#[derive(Debug, Clone)]
pub struct ProviderFactoryReopener<N: NodeTypesWithDB> {
    provider_factory: Arc<Mutex<ProviderFactory<N>>>,
    chain_spec: Arc<N::ChainSpec>,
    /// Paths to recreate the factory on reopen. `None` disables consistency checks and reopening
    /// (used by tests via [`Self::new_from_existing`]).
    reopen_paths: Option<ReopenPaths>,
    /// None ->No root hash (MockRootHasher)
    root_hash_config: Option<RootHashContext>,
    /// Keeps the static file directory watcher alive for as long as the reopener is in use, and is
    /// replaced (dropping the previous watcher, which stops its thread) when the factory is
    /// reopened. `None` when no watcher is active (factories built via [`Self::new_from_existing`],
    /// or if the watcher failed to start).
    static_file_watcher: Arc<Mutex<Option<RecommendedWatcher>>>,
}

/// root_hash_config None -> MockRootHasher used
impl<N: NodeTypesWithDB + ProviderNodeTypes + Clone> ProviderFactoryReopener<N> {
    pub fn new(
        db: N::DB,
        chain_spec: Arc<N::ChainSpec>,
        static_files_path: PathBuf,
        rocksdb_path: PathBuf,
        root_hash_config: Option<RootHashContext>,
    ) -> RethResult<Self> {
        let static_file_provider = StaticFileProvider::read_only(static_files_path.as_path())?;
        let static_file_watcher = start_static_file_index_watcher(
            static_file_provider.clone(),
            static_files_path.as_path(),
        );
        let provider_factory = ProviderFactory::new(
            db,
            chain_spec.clone(),
            static_file_provider,
            open_rocksdb_read_only(rocksdb_path.as_path())?,
            super::reth_task_runtime(),
        )?;

        Ok(Self {
            provider_factory: Arc::new(Mutex::new(provider_factory)),
            chain_spec,
            reopen_paths: Some(ReopenPaths {
                static_files_path,
                rocksdb_path,
            }),
            root_hash_config,
            static_file_watcher: Arc::new(Mutex::new(static_file_watcher)),
        })
    }

    pub fn new_from_existing(
        provider_factory: ProviderFactory<N>,
        root_hash_config: Option<RootHashContext>,
    ) -> RethResult<Self> {
        let chain_spec = provider_factory.chain_spec();
        Ok(Self {
            provider_factory: Arc::new(Mutex::new(provider_factory)),
            chain_spec,
            reopen_paths: None,
            root_hash_config,
            static_file_watcher: Arc::new(Mutex::new(None)),
        })
    }

    /// This will currently available provider factory without verifying if its correct, it can be used
    /// when consistency is not absolutely required
    pub fn provider_factory_unchecked(&self) -> ProviderFactory<N> {
        self.provider_factory.lock().clone()
    }

    /// This will check if historical block hashes for the given block is correct and if not it will reopen
    /// provider fatory.
    /// This should be used when consistency is required: e.g. building blocks.
    ///
    /// If the current block number is already known at the time of calling this method, you may pass it to
    /// avoid an additional DB lookup for the latest block number.
    pub fn check_consistency_and_reopen_if_needed(&self) -> eyre::Result<ProviderFactory<N>> {
        // Without reopen paths (factories built via `new_from_existing`) consistency checks are
        // disabled, since the factory cannot be recreated.
        let Some(reopen_paths) = &self.reopen_paths else {
            return Ok(self.provider_factory_unchecked());
        };

        let best_block_number = self
            .provider_factory_unchecked()
            .last_block_number()
            .map_err(|err| eyre::eyre!("Error getting best block number: {:?}", err))?;
        let mut provider_factory = self.provider_factory.lock();

        match check_block_hash_reader_health(best_block_number, provider_factory.deref_mut()) {
            Ok(()) => {}
            Err(err) => {
                debug!(?err, "Provider factory is inconsistent, reopening");
                inc_provider_reopen_counter();

                let static_file_provider =
                    StaticFileProvider::read_only(reopen_paths.static_files_path.as_path())?;
                let new_watcher = start_static_file_index_watcher(
                    static_file_provider.clone(),
                    reopen_paths.static_files_path.as_path(),
                );
                // Dropping the previous watcher stops its notify thread and releases the old
                // provider it captured.
                *self.static_file_watcher.lock() = new_watcher;
                *provider_factory = ProviderFactory::new(
                    provider_factory.db_ref().clone(),
                    self.chain_spec.clone(),
                    static_file_provider,
                    open_rocksdb_read_only(reopen_paths.rocksdb_path.as_path())?,
                    super::reth_task_runtime(),
                )?;
            }
        }

        match check_block_hash_reader_health(best_block_number, provider_factory.deref_mut()) {
            Ok(()) => {}
            Err(err) => {
                inc_provider_bad_reopen_counter();

                eyre::bail!(
                    "Provider factory is inconsistent after reopening: {:?}",
                    err
                );
            }
        }
        Ok(provider_factory.clone())
    }
}

/// reth's read-only [`StaticFileProvider`] no longer refreshes its in-memory index when the node
/// appends or truncates static files (reth v2.2 removed the `watch_directory` option and now
/// requires the caller to refresh manually). A non-node process such as rbuilder therefore observes
/// a frozen view of the chain head unless it refreshes the index itself.
///
/// This returns a [`RecommendedWatcher`] that watches the static files directory and calls
/// [`StaticFileProvider::initialize_index`] whenever a segment config file changes, restoring the
/// behavior reth previously provided internally. The watcher owns notify's event-loop thread and
/// the captured `provider`; dropping it stops that thread and releases the provider, so the caller
/// must keep it alive for as long as refreshes are needed. Returns `None` if the watcher cannot be
/// set up, in which case the index is simply not refreshed.
fn start_static_file_index_watcher<P: NodePrimitives>(
    provider: StaticFileProvider<P>,
    static_files_path: &Path,
) -> Option<RecommendedWatcher> {
    // notify invokes this callback on its own managed thread; no extra thread is needed here.
    let mut watcher = match RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            let event = match res {
                Ok(event) => event,
                Err(err) => {
                    warn!(?err, "Static file directory watch error");
                    return;
                }
            };
            if !matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                return;
            }
            // Segment config files ("*.conf") are rewritten when the node commits changes, so we
            // only refresh on those to avoid re-reading half-written data.
            let config_file_changed = event
                .paths
                .iter()
                .any(|path| path.extension().is_some_and(|ext| ext == "conf"));
            if !config_file_changed {
                return;
            }
            if let Err(err) = provider.initialize_index() {
                warn!(?err, "Failed to refresh static file provider index");
            }
        },
        NotifyConfig::default(),
    ) {
        Ok(watcher) => watcher,
        Err(err) => {
            error!(?err, "Failed to create static file directory watcher");
            return None;
        }
    };
    if let Err(err) = watcher.watch(static_files_path, RecursiveMode::NonRecursive) {
        error!(
            ?err,
            ?static_files_path,
            "Failed to watch static files directory"
        );
        return None;
    }
    Some(watcher)
}

/// Really ugly, should refactor with the string bellow or use better errors.
pub fn is_provider_factory_health_error(report: &eyre::Error) -> bool {
    report
        .to_string()
        .contains("Missing historical block hash for block")
}

#[derive(Debug, thiserror::Error)]
pub enum HistoricalBlockError {
    #[error("ProviderError while checking block hashes: {0}")]
    ProviderError(#[from] ProviderError),
    #[error("Missing historical block hash for block {missing_hash_block}, latest block: {latest_block}")]
    MissingHash {
        missing_hash_block: u64,
        latest_block: u64,
    },
}

/// Here we check if we have all the necessary historical block hashes in the database
/// This was added as a debugging method because static_files storage was not working correctly
/// last_block_number is the number of the latest committed block (i.e. if we build block 1001 it should be 1000)
pub fn check_block_hash_reader_health<R: BlockHashReader>(
    last_block_number: u64,
    reader: &R,
) -> Result<(), HistoricalBlockError> {
    // evm must have access to block hashes of 256 of the previous blocks
    let blocks_to_check = last_block_number.min(256);
    for i in 0..blocks_to_check {
        let num = last_block_number - i;
        let hash = reader.block_hash(num)?;
        if hash.is_none() {
            return Err(HistoricalBlockError::MissingHash {
                missing_hash_block: num,
                latest_block: last_block_number,
            });
        }
    }

    Ok(())
}

impl<N: NodeTypesWithDB + ProviderNodeTypes + Clone> StateProviderFactory
    for ProviderFactoryReopener<N>
where
    N::Primitives: NodePrimitives<BlockHeader = Header>,
{
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.latest()
    }

    fn history_by_block_number(&self, block: BlockNumber) -> ProviderResult<StateProviderBox> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.history_by_block_number(block)
    }

    fn history_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.history_by_block_hash(block)
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.best_block_number()
    }

    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.block_hash(number)
    }

    fn header(&self, block_hash: &BlockHash) -> ProviderResult<Option<Header>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.header(*block_hash)
    }

    fn header_by_number(&self, num: u64) -> ProviderResult<Option<Header>> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.header_by_number(num)
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        let provider = self
            .check_consistency_and_reopen_if_needed()
            .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))?;
        provider.last_block_number()
    }

    fn root_hasher(&self, parent_num_hash: BlockNumHash) -> ProviderResult<Box<dyn RootHasher>> {
        Ok(if let Some(root_hash_config) = &self.root_hash_config {
            let provider = self
                .check_consistency_and_reopen_if_needed()
                .map_err(|e| ProviderError::Database(DatabaseError::Other(e.to_string())))
                .unwrap();
            let parent_state_root = provider
                .header_by_hash_or_number(parent_num_hash.hash.into())?
                .map(|h| h.state_root);
            if parent_state_root.is_none() {
                error!("Parent hash is not found (for root_hasher)");
            }
            Box::new(RootHasherImpl::new(
                parent_num_hash,
                parent_state_root,
                root_hash_config.clone(),
                provider.clone(),
                provider,
            ))
        } else {
            Box::new(MockRootHasher {})
        })
    }
}

pub struct RootHasherImpl<T, HasherType> {
    parent_num_hash: BlockNumHash,
    provider: T,
    hasher: HasherType,
    sparse_trie_shared_cache: SparseTrieSharedCache,
    config: RootHashContext,
    runtime: reth_tasks::Runtime,
}

impl<T, HasherType> RootHasherImpl<T, HasherType> {
    pub fn new(
        parent_num_hash: BlockNumHash,
        parent_state_root: Option<B256>,
        config: RootHashContext,
        provider: T,
        hasher: HasherType,
    ) -> Self {
        let sparse_trie_shared_cache = SparseTrieSharedCache::new_with_parent_block_data(
            parent_num_hash.hash,
            parent_state_root.unwrap_or_default(),
        );
        Self {
            parent_num_hash,
            provider,
            hasher,
            config,
            sparse_trie_shared_cache,
            runtime: super::reth_task_runtime(),
        }
    }
}

impl<T, HasherType> RootHasher for RootHasherImpl<T, HasherType>
where
    HasherType: HashedPostStateProvider + Send + Sync,
    T: DatabaseProviderFactory<
            Provider: BlockReader
                          + StageCheckpointReader
                          + PruneCheckpointReader
                          + ChangeSetReader
                          + StorageChangeSetReader
                          + DBProvider
                          + BlockNumReader
                          + StorageSettingsCache,
        > + Send
        + Sync
        + Clone
        + 'static,
{
    fn run_prefetcher(&self, simulated_orders: mpsc::Receiver<SimulatedOrderCommand>) {
        run_trie_prefetcher(
            self.parent_num_hash,
            self.sparse_trie_shared_cache.clone(),
            self.config.sparse_mpt_version,
            self.provider.clone(),
            simulated_orders,
        );
    }

    fn account_proofs(
        &self,
        outcome: &BundleState,
        addresses: &utils::HashSet<Address>,
        local_ctx: &mut ThreadBlockBuildingContext,
    ) -> Result<utils::HashMap<Address, Vec<Bytes>>, RootHashError> {
        calculate_account_proofs(
            self.provider.clone(),
            self.parent_num_hash,
            outcome,
            addresses,
            &self.sparse_trie_shared_cache,
            &mut local_ctx.root_hash_calculator,
            &self.config,
        )
    }

    fn state_root(
        &self,
        outcome: &BundleState,
        incremental_change: &[Address],
        local_ctx: &mut ThreadBlockBuildingContext,
    ) -> Result<B256, RootHashError> {
        calculate_state_root(
            self.provider.clone(),
            &self.hasher,
            self.parent_num_hash,
            outcome,
            incremental_change,
            &self.sparse_trie_shared_cache,
            &mut local_ctx.root_hash_calculator,
            &self.config,
            self.runtime.clone(),
        )
    }
}

impl<T, HasherType> std::fmt::Debug for RootHasherImpl<T, HasherType> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootHasherImpl")
            .field("parent_num_hash", &self.parent_num_hash)
            .finish()
    }
}
