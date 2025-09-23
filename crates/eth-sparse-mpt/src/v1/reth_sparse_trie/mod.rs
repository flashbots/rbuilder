use alloy_primitives::B256;
use change_set::{prepare_change_set, prepare_change_set_for_prefetch};
use hash::RootHashError;
use reth_provider::{providers::ConsistentDbView, BlockReader, DatabaseProviderFactory};
use revm::database::BundleState;
use std::time::{Duration, Instant};

pub mod change_set;
pub mod hash;
pub mod shared_cache;
pub mod trie_fetcher;

use crate::{v1::sparse_mpt::AddNodeError, ChangedAccountData};

use self::trie_fetcher::*;

pub use self::shared_cache::SparseTrieSharedCache;

#[derive(Debug, Clone, Default)]
pub struct SparseTrieMetrics {
    pub change_set_time: Duration,
    pub gather_nodes_time: Duration,
    pub fetch_iterations: usize,
    pub missing_nodes: usize,
    pub fetched_nodes: usize,
    pub fetch_nodes_time: Duration,
    pub fill_cache_time: Duration,
    pub root_hash_time: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum SparseTrieError {
    #[error("Error while computing root hash: {0:?}")]
    RootHash(RootHashError),
    #[error("Error while fetching trie nodes from db: {0:?}")]
    FetchNode(#[from] FetchNodeError),
    #[error("Error while updated shared cache: {0:?}")]
    FailedToUpdateSharedCache(#[from] AddNodeError),
    /// This might indicate bug in the library
    /// or incorrect underlying storage (e.g. when deletes can't be applied to the trie because it does not have that keys)
    #[error("Failed to fetch data")]
    FailedToFetchData,
}

impl SparseTrieError {
    /// When reth's block tip changes we generate an error which is not really critical.
    /// We have this func so we can avoid logging it.
    /// This could be unstable due to reth changes, DON'T use as a real error check.
    pub fn is_db_consistency_error(&self) -> bool {
        matches!(
            self,
            SparseTrieError::FailedToUpdateSharedCache(AddNodeError::InconsistentProofs)
        )
    }
}

/// Prefetches data
#[allow(clippy::result_large_err)]
pub fn prefetch_tries_for_accounts<'a, Provider>(
    consistent_db_view: ConsistentDbView<Provider>,
    shared_cache: SparseTrieSharedCache,
    changed_data: impl Iterator<Item = &'a ChangedAccountData>,
) -> Result<SparseTrieMetrics, SparseTrieError>
where
    Provider: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync,
{
    let mut metrics = SparseTrieMetrics::default();

    let start = Instant::now();
    let change_set = prepare_change_set_for_prefetch(changed_data);
    metrics.change_set_time += start.elapsed();

    let fetcher = TrieFetcher::new(consistent_db_view);

    for _ in 0..3 {
        let start = Instant::now();
        let gather_result = shared_cache.gather_tries_for_changes(&change_set);
        metrics.gather_nodes_time += start.elapsed();

        let missing_nodes = match gather_result {
            Ok(_) => return Ok(metrics),
            Err(missing_nodes) => missing_nodes,
        };
        metrics.missing_nodes += missing_nodes.len();

        let start = Instant::now();
        let multiproof = fetcher.fetch_missing_nodes(missing_nodes)?;

        metrics.fetch_iterations += 1;
        metrics.fetch_nodes_time += start.elapsed();
        metrics.fetched_nodes += multiproof.len();

        let start = Instant::now();
        shared_cache.update_cache_with_fetched_nodes(multiproof)?;
        metrics.fill_cache_time += start.elapsed();
    }

    Err(SparseTrieError::FailedToFetchData)
}

/// Calculate root hash for the given outcome on top of the block defined by consistent_db_view.
/// * shared_cache should be created once for each parent block and it stores fetched parts of the trie
/// * It uses rayon for parallelism and the thread pool should be configured from outside.
pub fn calculate_root_hash_with_sparse_trie<Provider>(
    consistent_db_view: ConsistentDbView<Provider>,
    outcome: &BundleState,
    shared_cache: SparseTrieSharedCache,
) -> (Result<B256, SparseTrieError>, SparseTrieMetrics)
where
    Provider: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync,
{
    let mut metrics = SparseTrieMetrics::default();

    let fetcher = TrieFetcher::new(consistent_db_view);

    let start = Instant::now();
    let change_set = prepare_change_set(outcome.state.iter().map(|(a, acc)| (*a, acc)));
    metrics.change_set_time += start.elapsed();

    // {
    //     let change_set_json = serde_json::to_string_pretty(&change_set).expect("to json fail");
    //     let mut file = std::fs::File::create("/tmp/changeset.json").unwrap();
    //     file.write_all(change_set_json.as_bytes()).unwrap();
    // }

    for _ in 0..3 {
        let start = Instant::now();
        let gather_result = shared_cache.gather_tries_for_changes(&change_set);
        metrics.gather_nodes_time += start.elapsed();

        let missing_nodes = match gather_result {
            Ok(mut tries) => {
                return {
                    let start = Instant::now();
                    let root_hash_result = tries.calculate_root_hash(change_set, true, true);
                    metrics.root_hash_time += start.elapsed();
                    (root_hash_result.map_err(SparseTrieError::RootHash), metrics)
                }
            }
            Err(missing_nodes) => missing_nodes,
        };
        metrics.missing_nodes += missing_nodes.len();
        let start = Instant::now();
        let multiproof = match fetcher.fetch_missing_nodes(missing_nodes) {
            Ok(ok) => ok,
            Err(err) => return (Err(SparseTrieError::FetchNode(err)), metrics),
        };
        metrics.fetch_iterations += 1;

        // {
        //     let multiproof_json = serde_json::to_string_pretty(&multiproof).expect("to json fail");
        //     let mut file = std::fs::File::create(&format!("/tmp/multiproof_{}.json", i)).unwrap();
        //     file.write_all(multiproof_json.as_bytes()).unwrap();
        // }

        metrics.fetch_nodes_time += start.elapsed();
        metrics.fetched_nodes += multiproof.len();

        let start = Instant::now();
        if let Err(err) = shared_cache.update_cache_with_fetched_nodes(multiproof) {
            return (
                Err(SparseTrieError::FailedToUpdateSharedCache(err)),
                metrics,
            );
        };
        metrics.fill_cache_time += start.elapsed();
    }

    (Err(SparseTrieError::FailedToFetchData), metrics)
}
