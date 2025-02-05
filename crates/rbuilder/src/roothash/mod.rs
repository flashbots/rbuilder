mod prefetcher;

use alloy_primitives::B256;
use eth_sparse_mpt::{
    reth_sparse_trie::{
        calculate_root_hash_with_sparse_trie, trie_fetcher::FetchNodeError, SparseTrieError,
        SparseTrieSharedCache,
    },
    RootHashThreadPool,
};
use reth::providers::{providers::ConsistentDbView, ExecutionOutcome};
use reth_errors::ProviderError;
use reth_provider::{
    BlockReader, DatabaseProviderFactory, HashedPostStateProvider, StateCommitmentProvider,
};
use reth_trie::TrieInput;
use reth_trie_parallel::root::{ParallelStateRoot, ParallelStateRootError};
use tracing::trace;

pub use prefetcher::run_trie_prefetcher;

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
    #[error("Async state root: {0:?}")]
    AsyncStateRoot(#[from] ParallelStateRootError),
    #[error("Sparse state root: {0:?}")]
    SparseStateRoot(#[from] SparseTrieError),
    #[error("State root verification error")]
    Verification,
}

impl RootHashError {
    /// Error of this type means that db does not have trie for the required block
    /// This often happens when building for block after it was proposed.
    pub fn is_consistent_db_view_err(&self) -> bool {
        let provider_error = match self {
            RootHashError::AsyncStateRoot(ParallelStateRootError::Provider(p)) => p,
            RootHashError::SparseStateRoot(SparseTrieError::FetchNode(
                FetchNodeError::Provider(p),
            )) => p,
            _ => return false,
        };

        matches!(provider_error, ProviderError::ConsistentView(_))
    }
}

#[derive(Debug, Clone)]
pub struct RootHashContext {
    pub mode: RootHashMode,
    pub use_sparse_trie: bool,
    pub compare_sparse_trie_output: bool,
    pub thread_pool: Option<RootHashThreadPool>,
}

impl RootHashContext {
    pub fn new(
        use_sparse_trie: bool,
        compare_sparse_trie_output: bool,
        thread_pool: Option<RootHashThreadPool>,
    ) -> Self {
        Self {
            mode: RootHashMode::CorrectRoot,
            use_sparse_trie,
            compare_sparse_trie_output,
            thread_pool,
        }
    }
}

fn calculate_parallel_root_hash<P, HasherType>(
    hasher: &HasherType,
    outcome: &ExecutionOutcome,
    consistent_db_view: ConsistentDbView<P>,
) -> Result<B256, ParallelStateRootError>
where
    HasherType: HashedPostStateProvider,
    P: DatabaseProviderFactory<Provider: BlockReader>
        + StateCommitmentProvider
        + Send
        + Sync
        + Clone
        + 'static,
{
    let hashed_post_state = hasher.hashed_post_state(outcome.state());
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
    parent_hash: B256,
    outcome: &ExecutionOutcome,
    sparse_trie_shared_cache: SparseTrieSharedCache,
    config: &RootHashContext,
) -> Result<B256, RootHashError>
where
    HasherType: HashedPostStateProvider,
    P: DatabaseProviderFactory<Provider: BlockReader>
        + Send
        + Sync
        + Clone
        + StateCommitmentProvider
        + 'static,
{
    let consistent_db_view = match config.mode {
        RootHashMode::CorrectRoot => ConsistentDbView::new(provider.clone(), Some(parent_hash)),
        RootHashMode::IgnoreParentHash => ConsistentDbView::new_with_latest_tip(provider.clone())
            .map_err(ParallelStateRootError::Provider)?,
    };

    let reference_root_hash = if config.compare_sparse_trie_output {
        // parallel root hash uses rayon
        if let Some(thread_pool) = &config.thread_pool {
            thread_pool.rayon_pool.install(|| {
                calculate_parallel_root_hash(hasher, outcome, consistent_db_view.clone())
            })?
        } else {
            calculate_parallel_root_hash(hasher, outcome, consistent_db_view.clone())?
        }
    } else {
        B256::ZERO
    };

    let root = if config.use_sparse_trie {
        let (root, metrics) = calculate_root_hash_with_sparse_trie(
            consistent_db_view,
            outcome,
            sparse_trie_shared_cache,
            &config.thread_pool,
        );
        trace!(?metrics, "Sparse trie metrics");
        root?
    } else {
        calculate_parallel_root_hash(hasher, outcome, consistent_db_view)?
    };

    if config.compare_sparse_trie_output && reference_root_hash != root {
        return Err(RootHashError::Verification);
    }

    Ok(root)
}
