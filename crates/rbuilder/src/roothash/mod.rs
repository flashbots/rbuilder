use alloy_primitives::B256;
use reth::{
    providers::{providers::ConsistentDbView, BundleStateWithReceipts, ProviderFactory},
    tasks::pool::BlockingTaskPool,
};
use reth_db::database::Database;
use reth_trie_parallel::async_root::{AsyncStateRoot, AsyncStateRootError};

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

#[allow(clippy::too_many_arguments)]
pub fn calculate_state_root<DB: Database + Clone + 'static>(
    provider_factory: ProviderFactory<DB>,
    parent_hash: B256,
    bundle: &BundleStateWithReceipts,
    mode: RootHashMode,
    blocking_task_pool: BlockingTaskPool,
) -> Result<B256, AsyncStateRootError> {
    let consistent_db_view = match mode {
        RootHashMode::CorrectRoot => ConsistentDbView::new(provider_factory, Some(parent_hash)),
        RootHashMode::IgnoreParentHash => ConsistentDbView::new_with_latest_tip(provider_factory)
            .map_err(AsyncStateRootError::Provider)?,
        RootHashMode::SkipRootHash => {
            return Ok(B256::ZERO);
        }
    };

    let hashed_post_state = bundle.hash_state_slow();

    let async_root_calculator =
        AsyncStateRoot::new(consistent_db_view, blocking_task_pool, hashed_post_state);
    let root = futures::executor::block_on(async_root_calculator.incremental_root())?;

    Ok(root)
}
