mod prefetcher;
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, Bytes, B256};
use eth_sparse_mpt::*;
use reth::providers::providers::ConsistentDbView;
use reth_provider::{BlockReader, DatabaseProviderFactory, HashedPostStateProvider};
use reth_trie::TrieInput;
use reth_trie_parallel::root::{ParallelStateRoot, ParallelStateRootError};
use revm::database::BundleState;
use tracing::trace;

pub use prefetcher::run_trie_prefetcher;

use crate::telemetry::inc_root_hash_finalize_count;

#[derive(Debug, Clone, Copy)]
pub enum RootHashMode {
    /// Makes correct root hash calculation on the correct parent state.
    /// It must be used when building blocks.
    CorrectRoot,
    /// Makes correct root hash calculation on the incorrect parent state.
    /// It can be used for benchmarks.
    IgnoreParentHash,
}

#[derive(Debug, thiserror::Error)]
pub enum RootHashError {
    #[error("Database parent trie is not correct")]
    WrongDatabaseTrie,
    #[error("State root verification error")]
    Verification,
    #[error("Other {0}")]
    Other(#[from] eyre::Error),
}

impl RootHashError {
    /// Error of this type means that db does not have trie for the required block
    /// This often happens when building for block after it was proposed.
    pub fn is_consistent_db_view_err(&self) -> bool {
        matches!(self, RootHashError::WrongDatabaseTrie)
    }
}

#[derive(Debug, Clone)]
pub struct RootHashContext {
    pub mode: RootHashMode,
    pub use_sparse_trie: bool,
    pub sparse_mpt_version: ETHSpareMPTVersion,
    pub compare_sparse_trie_output: bool,
    pub thread_pool: Option<RootHashThreadPool>,
}

impl RootHashContext {
    pub fn new(
        use_sparse_trie: bool,
        compare_sparse_trie_output: bool,
        thread_pool: Option<RootHashThreadPool>,
        sparse_mpt_version: ETHSpareMPTVersion,
    ) -> Self {
        Self {
            mode: RootHashMode::CorrectRoot,
            use_sparse_trie,
            sparse_mpt_version,
            compare_sparse_trie_output,
            thread_pool,
        }
    }
}

pub fn calculate_account_proofs<P>(
    provider: P,
    parent_num_hash: BlockNumHash,
    outcome: &BundleState,
    addresses: &utils::HashSet<Address>,
    shared_cache: &SparseTrieSharedCache,
    local_cache: &mut SparseTrieLocalCache,
    config: &RootHashContext,
) -> Result<utils::HashMap<Address, Vec<Bytes>>, RootHashError>
where
    P: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync + Clone + 'static,
{
    let consistent_db_view = match config.mode {
        RootHashMode::CorrectRoot => ConsistentDbView::new(
            provider.clone(),
            Some((parent_num_hash.hash, parent_num_hash.number)),
        ),
        RootHashMode::IgnoreParentHash => ConsistentDbView::new_with_latest_tip(provider.clone())
            .map_err(|err| RootHashError::Other(err.into()))?,
    };

    let (result, metrics) = calculate_account_proofs_with_sparse_trie(
        consistent_db_view,
        outcome,
        addresses,
        shared_cache,
        local_cache,
        &config.thread_pool,
        config.sparse_mpt_version,
    );
    inc_root_hash_finalize_count(metrics.fetched_nodes);
    trace!(?metrics, "Sparse trie metrics");
    result.map_err(|error| match error {
        SparseTrieError::WrongDatabaseTrieError => RootHashError::WrongDatabaseTrie,
        SparseTrieError::Other(other) => RootHashError::Other(other),
    })
}

fn calculate_parallel_root_hash<P, HasherType>(
    hasher: &HasherType,
    outcome: &BundleState,
    consistent_db_view: ConsistentDbView<P>,
) -> Result<B256, ParallelStateRootError>
where
    HasherType: HashedPostStateProvider,
    P: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync + Clone + 'static,
{
    let hashed_post_state = hasher.hashed_post_state(outcome);
    let parallel_root_calculator = ParallelStateRoot::new(
        consistent_db_view.clone(),
        TrieInput::from_state(hashed_post_state),
    );
    parallel_root_calculator.incremental_root()
}

#[allow(clippy::too_many_arguments)]
pub fn calculate_state_root<P, HasherType>(
    provider: P,
    hasher: &HasherType,
    parent_num_hash: BlockNumHash,
    outcome: &BundleState,
    incremental_change: &[Address],
    shared_cache: &SparseTrieSharedCache,
    local_cache: &mut SparseTrieLocalCache,
    config: &RootHashContext,
) -> Result<B256, RootHashError>
where
    HasherType: HashedPostStateProvider,
    P: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync + Clone + 'static,
{
    let consistent_db_view = match config.mode {
        RootHashMode::CorrectRoot => ConsistentDbView::new(
            provider.clone(),
            Some((parent_num_hash.hash, parent_num_hash.number)),
        ),
        RootHashMode::IgnoreParentHash => ConsistentDbView::new_with_latest_tip(provider.clone())
            .map_err(|err| RootHashError::Other(err.into()))?,
    };

    let reference_root_hash = if config.compare_sparse_trie_output {
        // parallel root hash uses rayon
        if let Some(thread_pool) = &config.thread_pool {
            thread_pool
                .rayon_pool
                .install(|| {
                    calculate_parallel_root_hash(hasher, outcome, consistent_db_view.clone())
                })
                .map_err(|err| RootHashError::Other(err.into()))?
        } else {
            calculate_parallel_root_hash(hasher, outcome, consistent_db_view.clone())
                .map_err(|err| RootHashError::Other(err.into()))?
        }
    } else {
        B256::ZERO
    };

    let root = if config.use_sparse_trie {
        let (root, metrics) = calculate_root_hash_with_sparse_trie(
            consistent_db_view,
            outcome,
            incremental_change,
            shared_cache,
            local_cache,
            &config.thread_pool,
            config.sparse_mpt_version,
        );
        inc_root_hash_finalize_count(metrics.fetched_nodes);
        trace!(?metrics, "Sparse trie metrics");
        match root {
            Ok(hash) => hash,
            Err(SparseTrieError::WrongDatabaseTrieError) => {
                return Err(RootHashError::WrongDatabaseTrie);
            }
            Err(SparseTrieError::Other(other)) => {
                return Err(RootHashError::Other(other));
            }
        }
    } else {
        calculate_parallel_root_hash(hasher, outcome, consistent_db_view)
            .map_err(|err| RootHashError::Other(err.into()))?
    };

    if config.compare_sparse_trie_output && reference_root_hash != root {
        return Err(RootHashError::Verification);
    }

    Ok(root)
}
