use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

use ahash::RandomState;
use alloy_primitives::{Address, B256, I256, U256};
use dashmap::DashMap;
use itertools::Itertools;
use rbuilder_primitives::evm_inspector::UsedStateTrace;
use result_store::{ActionResult, ExecutionResultStore, NextAction};
use reth_errors::ProviderError;
use revm::{context::result::ResultAndState, state::AccountInfo, Database};
use tracing::info;

use crate::utils::signed_uint_delta;

use super::{CriticalCommitOrderError, TransactionErr};

mod evm_db;
mod result_store;

pub use evm_db::*;

const NUM_OF_CACHED_EXECUTIONS: usize = 5;
const PREALLOCATED_TX_CACHE_CAPACITY: usize = 10_000;

#[derive(Debug)]
pub struct CachedExecutionResult {
    pub tx_hash: B256,
    // we store coinbase because builders can use different coinbase (e.g. fee recepient and builder coinbase)
    pub coinbase: Address,
    pub recorded_trace: TxStateAccessTrace,
    pub result: Result<ResultAndState, TransactionErr>,
    // we always collect used state trace to detect direct reads of coinbase balance
    pub used_state_trace: Arc<UsedStateTrace>,
}

#[derive(Debug)]
pub struct CachingResult {
    /// None means cache miss
    pub result: Option<Result<ResultAndState, TransactionErr>>,
    pub used_state_trace: Arc<UsedStateTrace>,
    pub should_cache: bool,
}

impl CachingResult {
    pub fn new_cache_miss(should_cache: bool) -> Self {
        Self {
            result: None,
            used_state_trace: Arc::new(UsedStateTrace::default()),
            should_cache,
        }
    }
}

#[derive(Clone, Debug)]
struct TxCacheEntryResult {
    read_coinbase_account: Option<Option<AccountInfo>>,
    result: Result<ResultAndState, TransactionErr>,
    used_state_trace: Arc<UsedStateTrace>,
}

#[derive(Debug, Default)]
struct TxCacheEntry {
    store: ExecutionResultStore<TxCacheEntryResult>,
    count_total_executions: AtomicUsize,
    count_hits: AtomicUsize,
    never_cache: AtomicBool,
}

impl TxCacheEntry {
    fn has_storage_capacity(&self) -> bool {
        self.store.len() < NUM_OF_CACHED_EXECUTIONS
    }

    fn never_cache(&self) -> bool {
        self.never_cache.load(Ordering::Relaxed)
    }

    fn store_result(&self, result: CachedExecutionResult) {
        if !self.has_storage_capacity() {
            return;
        }

        if trace_is_not_cacheable(&result.used_state_trace, &result.coinbase) {
            self.never_cache.store(true, Ordering::Relaxed);
            return;
        }

        let mut trace = result.recorded_trace.trace;
        let coinbase_read_entry = trace
            .iter()
            .find_position(|r| match r {
                AccessRecord::Account { address, .. } => address == &result.coinbase,
                _ => false,
            })
            .map(|(idx, _)| idx);
        let coinbase_read_entry = if let Some(idx) = coinbase_read_entry {
            match trace.remove(idx) {
                AccessRecord::Account { result, .. } => Some(result),
                _ => unreachable!(),
            }
        } else {
            None
        };

        let result = TxCacheEntryResult {
            read_coinbase_account: coinbase_read_entry,
            result: result.result,
            used_state_trace: result.used_state_trace,
        };
        self.store.insert_result(trace, result);
    }

    fn inc_total_executions(&self) {
        self.count_total_executions.fetch_add(1, Ordering::Relaxed);
    }

    fn total_executions(&self) -> usize {
        self.count_total_executions.load(Ordering::Relaxed)
    }

    fn inc_hits(&self) {
        self.count_hits.fetch_add(1, Ordering::Relaxed);
    }

    fn hits(&self) -> usize {
        self.count_hits.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub struct TxExecutionCache {
    enabled: bool,
    // (tx_hash, coinbase) -> CachedResult
    stored_results: DashMap<(B256, Address), Arc<TxCacheEntry>, RandomState>,
    cache_hits: Arc<AtomicUsize>,
    cache_asks: Arc<AtomicUsize>,
}

impl Drop for TxExecutionCache {
    fn drop(&mut self) {
        let total = self.cache_asks.load(Ordering::Relaxed);
        let hits = self.cache_hits.load(Ordering::Relaxed);
        if total == 0 {
            return;
        }

        let rate = hits as f64 / total as f64;

        let mut perfectly_cached_count = 0;
        let mut tx_count = 0;
        let mut sum_total_executions = 0;
        for entry in self.stored_results.iter() {
            tx_count += 1;
            let hits = entry.hits();
            let total_executions = entry.total_executions();
            sum_total_executions += total_executions;
            let misses = total_executions - hits;
            if misses <= NUM_OF_CACHED_EXECUTIONS {
                perfectly_cached_count += 1;
            }
        }

        let theoretical_max_rate =
            (sum_total_executions - tx_count) as f64 / sum_total_executions as f64;

        let perfectly_cached_rate = perfectly_cached_count as f64 / tx_count as f64;

        info!(
            rate,
            theoretical_max_rate,
            sum_total_executions,
            tx_count,
            perfectly_cached_rate,
            "EVM tx cache"
        );
    }
}

impl TxExecutionCache {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            stored_results: DashMap::with_capacity_and_hasher(
                PREALLOCATED_TX_CACHE_CAPACITY,
                RandomState::default(),
            ),
            cache_hits: Default::default(),
            cache_asks: Default::default(),
        }
    }
}

impl TxExecutionCache {
    fn get_tx_entry(&self, tx_hash: B256, coinbase: Address) -> Arc<TxCacheEntry> {
        let key = (tx_hash, coinbase);
        if let Some(entry) = self.stored_results.get(&key) {
            return entry.value().clone();
        }
        // we use get first and entry last because entry API uses write lock even if entry is occupied
        self.stored_results.entry(key).or_default().value().clone()
    }

    pub fn store_result(&self, result: CachedExecutionResult) {
        if !self.enabled {
            return;
        }
        let entry = self.get_tx_entry(result.tx_hash, result.coinbase);

        entry.store_result(result);
    }

    fn inc_cache_asks(&self) {
        self.cache_asks.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_cache_hits(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get_cached_result(
        &self,
        mut db: impl Database<Error = ProviderError>,
        tx_hash: &B256,
        coinbase: &Address,
    ) -> Result<CachingResult, CriticalCommitOrderError> {
        if !self.enabled {
            return Ok(CachingResult::new_cache_miss(false));
        }

        self.inc_cache_asks();
        let entry = self.get_tx_entry(*tx_hash, *coinbase);
        entry.inc_total_executions();

        if entry.never_cache() {
            return Ok(CachingResult::new_cache_miss(false));
        }

        let cached_result;
        let mut walker = entry.store.get_walker();
        loop {
            let next_action = match walker.next_action() {
                Some(action) => action,
                None => {
                    return Ok(CachingResult::new_cache_miss(entry.has_storage_capacity()));
                }
            };
            let action_result = match next_action {
                NextAction::CheckAccount(address) => {
                    let current_account = db.basic(address)?;

                    ActionResult::AccountValue(current_account)
                }
                NextAction::CheckStorage(address, index) => {
                    db.basic(address)?; // we load account here because revm database panics if we never loaded account before slot
                    let current_value = db.storage(address, index)?;
                    ActionResult::StorageValue(current_value)
                }
                NextAction::DoNothing => ActionResult::DoNothing,
            };
            if let Some(res) = walker.action_result(&action_result) {
                cached_result = res;
                break;
            }
        }

        let mut evm_result = match cached_result.result {
            Ok(res) => res,
            Err(_) => {
                // if tx execution was error (tx not included, we can ingore coinbase handling)
                self.inc_cache_hits();
                entry.inc_hits();
                return Ok(CachingResult {
                    result: Some(cached_result.result),
                    used_state_trace: cached_result.used_state_trace,
                    should_cache: false,
                });
            }
        };

        let used_coinbase = if let Some(account) = cached_result.read_coinbase_account {
            account
        } else {
            // its possible to not read coinbase if tx execution is error, but that case should have been handled above
            panic!("tx_sim_cache: tx did not read coinbase account");
        };

        let current_coinbase = db.basic(*coinbase)?;

        let mut coinbase_nonce_delta = 0i64;
        let mut coinbase_balance_delta = I256::ZERO;
        match (used_coinbase, current_coinbase) {
            (None, None) => {
                // coinbase cached is none and current is none, do nothing
            }
            (None, Some(_)) | (Some(_), None) => {
                // coinbase was created or destoyed
                // for caution lets just reject these cases
                return Ok(CachingResult::new_cache_miss(entry.has_storage_capacity()));
            }
            (Some(cached_coinbase), Some(current_coinbase)) => {
                if cached_coinbase.code_hash != current_coinbase.code_hash {
                    // thats a strange case that we want to just ignore
                    return Ok(CachingResult::new_cache_miss(entry.has_storage_capacity()));
                }
                // if nonce changed we want to modify nonce in the result by that amount
                coinbase_nonce_delta = current_coinbase.nonce as i64 - cached_coinbase.nonce as i64;
                coinbase_balance_delta =
                    signed_uint_delta(current_coinbase.balance, cached_coinbase.balance);
            }
        }

        let coinbase_account = match evm_result.state.get_mut(coinbase) {
            Some(acc) => acc,
            None => {
                // strange case when coinbase account was not modified, rejecting just in case
                return Ok(CachingResult::new_cache_miss(entry.has_storage_capacity()));
            }
        };

        match coinbase_account
            .info
            .nonce
            .checked_add_signed(coinbase_nonce_delta)
        {
            Some(new_nonce) => coinbase_account.info.nonce = new_nonce,
            None => {
                return Ok(CachingResult::new_cache_miss(entry.has_storage_capacity()));
            }
        };

        if !checked_add_balance(&mut coinbase_account.info.balance, &coinbase_balance_delta) {
            return Ok(CachingResult::new_cache_miss(entry.has_storage_capacity()));
        }

        self.inc_cache_hits();
        entry.inc_hits();
        Ok(CachingResult {
            result: Some(Ok(evm_result)),
            used_state_trace: cached_result.used_state_trace,
            should_cache: false,
        })
    }
}

fn checked_add_balance(balance: &mut U256, delta: &I256) -> bool {
    let negative = delta.is_negative();
    let delta_abs: U256 = delta.abs().try_into().unwrap();
    if negative && &delta_abs > balance {
        return false;
    }

    let result = if negative {
        balance.checked_sub(delta_abs)
    } else {
        balance.checked_add(delta_abs)
    };
    match result {
        Some(res) => {
            *balance = res;
            true
        }
        None => false,
    }
}

// check if given execution trace actually read coinbase balance or writes to coinbase storage
// if so we can't cache this tx as it can mess cache
fn trace_is_not_cacheable(used_state_trace: &UsedStateTrace, coinbase: &Address) -> bool {
    let reads_balance = used_state_trace.read_balances.contains_key(coinbase);
    let writes_slots = used_state_trace
        .written_slot_values
        .keys()
        .any(|slot_key| &slot_key.address == coinbase);
    reads_balance || writes_slots
}
