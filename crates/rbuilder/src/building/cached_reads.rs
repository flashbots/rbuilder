//! Caching layer for database, used to minimize disc access.
//! The cache is shared between threads.

use alloy_primitives::{Address, B256, U256};
use dashmap::DashMap;
use revm::{bytecode::Bytecode, state::AccountInfo, Database};
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

#[derive(Debug)]
pub struct CachedDB<'b, DB> {
    db: DB,
    shared_cache: &'b SharedCachedReads,
}

impl<'b, DB> CachedDB<'b, DB> {
    pub fn new(db: DB, shared_cache: &'b SharedCachedReads) -> Self {
        Self { db, shared_cache }
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

impl<DB: Database> Database for CachedDB<'_, DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(data) = self.shared_cache.account_info.get(&address) {
            self.inc_shared_hit();
            return Ok(data.clone());
        }
        self.inc_shared_miss();
        let result = self.db.basic(address)?;
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
        let data = self.db.code_by_hash(code_hash)?;
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
        let result = self.db.storage(address, index)?;
        self.shared_cache.storage.insert((address, index), result);
        Ok(result)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        if let Some(data) = self.shared_cache.block_hash.get(&number) {
            self.inc_shared_hit();
            return Ok(*data);
        }
        self.inc_shared_miss();
        let data = self.db.block_hash(number)?;
        self.shared_cache.block_hash.insert(number, data);
        Ok(data)
    }
}
