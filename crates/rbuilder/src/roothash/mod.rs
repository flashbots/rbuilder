use alloy_primitives::B256;
use eth_sparse_mpt::reth_sparse_trie::{
    calculate_root_hash_with_sparse_trie, trie_fetcher::FetchNodeError, SparseTrieError,
    SparseTrieSharedCache,
};
use reth::{
    providers::{providers::ConsistentDbView, ExecutionOutcome, ProviderFactory},
    tasks::pool::BlockingTaskPool,
};
use reth_db::database::Database;
use reth_errors::ProviderError;
use reth_trie_parallel::async_root::{AsyncStateRoot, AsyncStateRootError};
use tracing::trace;

#[derive(Debug, Clone, Copy)]
pub enum RootHashMode {
    /// Makes correct root hash calculation on the correct parent state.
    /// It must be used when building blocks.
    CorrectRoot,
    /// Makes correct root hash calculation on the incorrect parent state.
    /// It can be used for benchmarks.
    IgnoreParentHash,
    /// Don't calculate root hash.
    /// It can be used for backtest.
    SkipRootHash,
}

#[derive(Debug, thiserror::Error)]
pub enum RootHashError {
    #[error("Async state root: {0:?}")]
    AsyncStateRoot(#[from] AsyncStateRootError),
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
            RootHashError::AsyncStateRoot(AsyncStateRootError::Provider(p)) => p,
            RootHashError::SparseStateRoot(SparseTrieError::FetchNode(
                FetchNodeError::Provider(p),
            )) => p,
            _ => return false,
        };

        matches!(provider_error, ProviderError::ConsistentView(_))
    }
}

#[derive(Debug, Clone)]
pub struct RootHashConfig {
    pub mode: RootHashMode,
    pub use_sparse_trie: bool,
    pub compare_sparse_trie_output: bool,
}

impl RootHashConfig {
    pub fn skip_root_hash() -> Self {
        Self {
            mode: RootHashMode::SkipRootHash,
            use_sparse_trie: false,
            compare_sparse_trie_output: false,
        }
    }

    pub fn live_config(use_sparse_trie: bool, compare_sparse_trie_output: bool) -> Self {
        Self {
            mode: RootHashMode::CorrectRoot,
            use_sparse_trie,
            compare_sparse_trie_output,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn calculate_state_root<DB: Database + Clone + 'static>(
    provider_factory: ProviderFactory<DB>,
    parent_hash: B256,
    outcome: &ExecutionOutcome,
    blocking_task_pool: BlockingTaskPool,
    sparse_trie_shared_cache: SparseTrieSharedCache,
    config: RootHashConfig,
) -> Result<B256, RootHashError> {
    let consistent_db_view = match config.mode {
        RootHashMode::CorrectRoot => ConsistentDbView::new(provider_factory, Some(parent_hash)),
        RootHashMode::IgnoreParentHash => ConsistentDbView::new_with_latest_tip(provider_factory)
            .map_err(AsyncStateRootError::Provider)?,
        RootHashMode::SkipRootHash => {
            return Ok(B256::ZERO);
        }
    };

    let reference_root_hash = if config.compare_sparse_trie_output {
        let hashed_post_state = outcome.hash_state_slow();

        let async_root_calculator = AsyncStateRoot::new(
            consistent_db_view.clone(),
            blocking_task_pool.clone(),
            hashed_post_state.clone(),
        );

        futures::executor::block_on(async_root_calculator.incremental_root())?
    } else {
        B256::ZERO
    };

    let root = if config.use_sparse_trie {
        let (root, metrics) = calculate_root_hash_with_sparse_trie(
            consistent_db_view,
            outcome,
            sparse_trie_shared_cache,
        );
        trace!(?metrics, "Sparse trie metrics");
        root?
    } else {
        let hashed_post_state = outcome.hash_state_slow();

        let async_root_calculator =
            AsyncStateRoot::new(consistent_db_view, blocking_task_pool, hashed_post_state);

        futures::executor::block_on(async_root_calculator.incremental_root())?
    };

    if config.compare_sparse_trie_output && reference_root_hash != root {
        return Err(RootHashError::Verification);
    }

    Ok(root)
}
