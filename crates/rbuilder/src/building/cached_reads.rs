//! Caching layer for database, used to minimize disc access.
//! The cache is shared between threads.

use alloy_primitives::{Address, B256, U256};
use dashmap::DashMap;
use reth::revm::database::StateProviderDatabase;
use reth_errors::ProviderError;
use reth_provider::StateProvider;
use revm::{bytecode::Bytecode, state::AccountInfo, Database as RevmDatabase};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use ahash::RandomState;
use tracing::info;

/// Database cache shared bewteen multiple threads.
/// It should be created for unique parent block.
#[derive(Debug, Clone, Default)]
pub struct SharedCachedReads {
    pub account_info: DashMap<Address, Option<AccountInfo>, RandomState>,
    pub storage: DashMap<(Address, U256), U256, RandomState>,

    pub code_by_hash: DashMap<B256, Bytecode, RandomState>,
    pub block_hash: DashMap<u64, B256, RandomState>,

    pub shared_hit_count: Arc<AtomicU64>,
    pub shared_miss_count: Arc<AtomicU64>,
}

impl Drop for SharedCachedReads {
    fn drop(&mut self) {
        let shared_hit_count = self.shared_hit_count.load(Ordering::Relaxed);
        let shared_miss_count = self.shared_miss_count.load(Ordering::Relaxed);
        let shared_hit_perc =
            100.0 * shared_hit_count as f64 / (shared_hit_count + shared_miss_count) as f64;
        info!(
            shared_hit_count,
            shared_miss_count, shared_hit_perc, "Storage cache stats"
        );
    }
}

/// Database that wraps a reth state provider with a shared read cache.
#[derive(Clone)]
pub struct CachedDB {
    state_provider: Arc<dyn StateProvider + Send + Sync>,
    shared_cache: Arc<SharedCachedReads>,
}

impl std::fmt::Debug for CachedDB {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedDB").finish_non_exhaustive()
    }
}

impl CachedDB {
    pub fn new(
        state_provider: Arc<dyn StateProvider + Send + Sync>,
        shared_cache: Arc<SharedCachedReads>,
    ) -> Self {
        Self {
            state_provider,
            shared_cache,
        }
    }

    fn inner_db(&self) -> StateProviderDatabase<&dyn StateProvider> {
        StateProviderDatabase::new(&*self.state_provider)
    }

    fn inc_shared_hit(&self) {
        self.shared_cache
            .shared_hit_count
            .fetch_add(1, Ordering::Relaxed);
    }

    fn inc_shared_miss(&self) {
        self.shared_cache
            .shared_miss_count
            .fetch_add(1, Ordering::Relaxed);
    }
}

impl RevmDatabase for CachedDB {
    type Error = ProviderError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(data) = self.shared_cache.account_info.get(&address) {
            self.inc_shared_hit();
            return Ok(data.clone());
        }
        self.inc_shared_miss();
        let result = self.inner_db().basic(address)?;
        self.shared_cache
            .account_info
            .insert(address, result.clone());
        Ok(result)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(data) = self.shared_cache.code_by_hash.get(&code_hash) {
            self.inc_shared_hit();
            return Ok(data.clone());
        }
        self.inc_shared_miss();
        let data = self.inner_db().code_by_hash(code_hash)?;
        self.shared_cache
            .code_by_hash
            .insert(code_hash, data.clone());
        Ok(data)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(data) = self.shared_cache.storage.get(&(address, index)) {
            self.inc_shared_hit();
            return Ok(*data);
        }
        self.inc_shared_miss();
        let result = self.inner_db().storage(address, index)?;
        self.shared_cache.storage.insert((address, index), result);
        Ok(result)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        if let Some(data) = self.shared_cache.block_hash.get(&number) {
            self.inc_shared_hit();
            return Ok(*data);
        }
        self.inc_shared_miss();
        let data = self.inner_db().block_hash(number)?;
        self.shared_cache.block_hash.insert(number, data);
        Ok(data)
    }
}
