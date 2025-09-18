//! This library is useful when you need to calculate Ethereum root hash many times on top of the same parent block using reth database.

#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::len_without_is_empty)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::type_complexity)]

use crate::utils::{HashMap, HashSet};
use alloy_primitives::{Address, Bytes, B256};
use reth_provider::{providers::ConsistentDbView, BlockReader, DatabaseProviderFactory};
use revm::database::BundleState;
use std::sync::Arc;

#[cfg(any(test, feature = "benchmark-utils"))]
pub mod test_utils;
pub mod utils;

pub mod v1;
pub mod v2;

#[derive(Debug)]
pub struct ChangedAccountData {
    pub address: Address,
    pub account_deleted: bool,
    /// (slot, deleted)
    pub slots: Vec<(B256, bool)>,
}

impl ChangedAccountData {
    pub fn new(address: Address, account_deleted: bool) -> Self {
        Self {
            address,
            account_deleted,
            slots: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RootHashThreadPool {
    pub rayon_pool: Arc<rayon::ThreadPool>,
}

impl RootHashThreadPool {
    pub fn try_new(threads: usize) -> Result<RootHashThreadPool, rayon::ThreadPoolBuildError> {
        let rayon_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|idx| format!("sparse_mpt:{idx}"))
            .build()?;
        Ok(RootHashThreadPool {
            rayon_pool: Arc::new(rayon_pool),
        })
    }
}

impl Default for RootHashThreadPool {
    fn default() -> Self {
        let cpus = rayon::current_num_threads();
        Self::try_new(cpus).expect("failed to create default root hash threadpool")
    }
}

#[derive(Debug, Default, Clone)]
pub struct SparseTrieSharedCache {
    cache_v1: v1::reth_sparse_trie::SparseTrieSharedCache,
    cache_v2: v2::SharedCacheV2,
}

impl SparseTrieSharedCache {
    pub fn new_with_parent_block_data(parent_block_hash: B256, parent_state_root: B256) -> Self {
        let cache_v1 = v1::reth_sparse_trie::SparseTrieSharedCache::new_with_parent_state_root(
            parent_state_root,
        );
        let mut cache_v2 = v2::SharedCacheV2::default();
        cache_v2.last_block_hash = parent_block_hash;
        Self { cache_v1, cache_v2 }
    }
}

#[derive(Debug, Default, Clone)]
pub struct SparseTrieLocalCache {
    calc: v2::RootHashCalculator,
}

#[derive(Debug, Clone, Copy)]
pub enum ETHSpareMPTVersion {
    V1,
    V2,
}

pub fn prefetch_tries_for_accounts<'a, Provider>(
    consistent_db_view: ConsistentDbView<Provider>,
    shared_cache: &SparseTrieSharedCache,
    changed_data: impl Iterator<Item = &'a ChangedAccountData>,
    version: ETHSpareMPTVersion,
) -> Result<SparseTrieMetrics, SparseTrieError>
where
    Provider: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync,
{
    match version {
        ETHSpareMPTVersion::V1 => {
            let mut metrics = SparseTrieMetrics::default();
            v1::reth_sparse_trie::prefetch_tries_for_accounts(
                consistent_db_view,
                shared_cache.cache_v1.clone(),
                changed_data,
            )
            .map(|metrics_v1| {
                metrics.fetched_nodes = metrics_v1.fetched_nodes;
                metrics
            })
            .map_err(|err| SparseTrieError::Other(err.into()))
        }
        ETHSpareMPTVersion::V2 => {
            v2::prefetch_proofs(consistent_db_view, &shared_cache.cache_v2, changed_data)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SparseTrieMetrics {
    pub fetched_nodes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SparseTrieError {
    #[error("Wrong database trie")]
    WrongDatabaseTrieError,
    #[error("Other {0}")]
    Other(#[from] eyre::Error),
}

impl SparseTrieError {
    pub fn other<E: Into<eyre::Error>>(e: E) -> Self {
        Self::Other(e.into())
    }
}

pub fn calculate_account_proofs_with_sparse_trie<Provider>(
    consistent_db_view: ConsistentDbView<Provider>,
    outcome: &BundleState,
    proof_targets: &HashSet<Address>,
    shared_cache: &SparseTrieSharedCache,
    local_cache: &mut SparseTrieLocalCache,
    thread_pool: &Option<RootHashThreadPool>,
    version: ETHSpareMPTVersion,
) -> (
    Result<HashMap<Address, Vec<Bytes>>, SparseTrieError>,
    SparseTrieMetrics,
)
where
    Provider: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync,
{
    let calculate = || match version {
        ETHSpareMPTVersion::V1 => (
            Err(SparseTrieError::Other(eyre::eyre!(
                "proof generation not supported in v1"
            ))),
            Default::default(),
        ),
        ETHSpareMPTVersion::V2 => {
            let result = local_cache.calc.calculate_root_hash_with_sparse_trie(
                consistent_db_view,
                shared_cache.cache_v2.clone(),
                outcome,
                &[],
                proof_targets,
            );
            match result {
                Ok((_, proofs, metrics)) => (Ok(proofs), metrics),
                Err(err) => (Err(err), Default::default()),
            }
        }
    };
    if let Some(thread_pool) = thread_pool {
        thread_pool.rayon_pool.install(calculate)
    } else {
        calculate()
    }
}

pub fn calculate_root_hash_with_sparse_trie<Provider>(
    consistent_db_view: ConsistentDbView<Provider>,
    outcome: &BundleState,
    incremental_change: &[Address],
    shared_cache: &SparseTrieSharedCache,
    local_cache: &mut SparseTrieLocalCache,
    thread_pool: &Option<RootHashThreadPool>,
    version: ETHSpareMPTVersion,
) -> (Result<B256, SparseTrieError>, SparseTrieMetrics)
where
    Provider: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync,
{
    if let Some(thread_pool) = thread_pool {
        thread_pool.rayon_pool.install(|| {
            calculate_root_hash_with_sparse_trie_internal(
                consistent_db_view,
                outcome,
                incremental_change,
                shared_cache,
                local_cache,
                version,
            )
        })
    } else {
        calculate_root_hash_with_sparse_trie_internal(
            consistent_db_view,
            outcome,
            incremental_change,
            shared_cache,
            local_cache,
            version,
        )
    }
}

pub fn calculate_root_hash_with_sparse_trie_internal<Provider>(
    consistent_db_view: ConsistentDbView<Provider>,
    outcome: &BundleState,
    incremental_change: &[Address],
    shared_cache: &SparseTrieSharedCache,
    local_cache: &mut SparseTrieLocalCache,
    version: ETHSpareMPTVersion,
) -> (Result<B256, SparseTrieError>, SparseTrieMetrics)
where
    Provider: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync,
{
    match version {
        ETHSpareMPTVersion::V1 => {
            let (result, metrics_v1) = v1::reth_sparse_trie::calculate_root_hash_with_sparse_trie(
                consistent_db_view,
                outcome,
                shared_cache.cache_v1.clone(),
            );
            let result = result.map_err(|err| SparseTrieError::Other(err.into()));
            let mut metrics = SparseTrieMetrics::default();
            metrics.fetched_nodes = metrics_v1.fetched_nodes;
            (result, metrics)
        }
        ETHSpareMPTVersion::V2 => {
            let result = local_cache.calc.calculate_root_hash_with_sparse_trie(
                consistent_db_view,
                shared_cache.cache_v2.clone(),
                outcome,
                incremental_change,
                &Default::default(),
            );
            match result {
                Ok((res, _, metrics)) => (Ok(res), metrics),
                Err(err) => (Err(err), Default::default()),
            }
        }
    }
}
