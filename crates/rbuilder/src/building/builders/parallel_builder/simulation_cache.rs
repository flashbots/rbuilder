use crate::primitives::OrderId;
use ahash::HashMap;
use alloy_primitives::U256;
use parking_lot::RwLock as PLRwLock;
use reth_payload_builder::database::CachedReads;
use revm::db::BundleState;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

/// An instance of a simulation result that has been cached.
#[derive(Debug, Clone)]
pub struct CachedSimulationState {
    pub cached_reads: CachedReads,
    pub bundle_state: BundleState,
    pub total_profit: U256,
    pub per_order_profits: Vec<(OrderId, U256)>,
}

/// An inner cache of simulation results, keyed by the ordering of the orders that produced the simulation state.
#[derive(Debug, Default)]
struct SimulationCache {
    inner_cache: HashMap<Vec<OrderId>, Arc<CachedSimulationState>>,
}

impl SimulationCache {
    /// Creates a new `SimulationCache`.
    pub fn new() -> Self {
        SimulationCache {
            inner_cache: HashMap::default(),
        }
    }
}

/// A shared instance of the simulation cache that can be used by many threads.
/// It provides thread-safe access and maintains statistics about cache usage.
#[derive(Debug, Clone)]
pub struct SharedSimulationCache {
    cache: Arc<PLRwLock<SimulationCache>>,
    cache_requests: Arc<AtomicUsize>,
    full_hits: Arc<AtomicUsize>,
    partial_hits: Arc<AtomicUsize>,
    number_of_order_simulations_saved: Arc<AtomicUsize>,
    number_of_order_simulations_requested: Arc<AtomicUsize>,
}

impl SharedSimulationCache {
    /// Creates a new `SharedSimulationCache`.
    pub fn new() -> Self {
        SharedSimulationCache {
            cache: Arc::new(PLRwLock::new(SimulationCache::new())),
            cache_requests: Arc::new(AtomicUsize::new(0)),
            full_hits: Arc::new(AtomicUsize::new(0)),
            partial_hits: Arc::new(AtomicUsize::new(0)),
            number_of_order_simulations_saved: Arc::new(AtomicUsize::new(0)),
            number_of_order_simulations_requested: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Retrieves the cached simulation state for the longest matching prefix of the given ordering.
    ///
    /// # Arguments
    ///
    /// * `ordering` - A reference to a vector of `OrderId`s representing the sequence of orders.
    ///
    /// # Returns
    ///
    /// A tuple containing:
    /// - An `Option` with the `Arc<CachedSimulationState>` if a matching prefix is found.
    /// - The index up to which the ordering was cached.
    pub fn get_cached_state(
        &self,
        ordering: &[OrderId],
    ) -> (Option<Arc<CachedSimulationState>>, usize) {
        let total_requested = ordering.len();
        self.number_of_order_simulations_requested
            .fetch_add(total_requested, Ordering::Relaxed);
        self.cache_requests.fetch_add(1, Ordering::Relaxed);

        let cache_lock = self.cache.read();
        let mut current_state: Option<Arc<CachedSimulationState>> = None;
        let mut last_cached_index = 0;

        let mut partial_key = ordering.to_owned();

        while !partial_key.is_empty() {
            if let Some(cached_result) = cache_lock.inner_cache.get(&partial_key) {
                current_state = Some(cached_result.clone());
                last_cached_index = partial_key.len();

                // Update statistics
                if last_cached_index == ordering.len() {
                    self.full_hits.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.partial_hits.fetch_add(1, Ordering::Relaxed);
                }
                self.number_of_order_simulations_saved
                    .fetch_add(last_cached_index, Ordering::Relaxed);

                break;
            }

            // Remove the last `OrderId` and try again with a shorter partial key
            partial_key.pop();
        }

        (current_state, last_cached_index)
    }

    /// Stores the simulation result for a given ordering in the shared cache.
    ///
    /// # Arguments
    ///
    /// * `ordering` - A reference to a vector of `OrderId`s representing the sequence of orders.
    /// * `cached_state` - The `CachedSimulationState` to store.
    pub fn store_cached_state(&self, ordering: &[OrderId], cached_state: CachedSimulationState) {
        let partial_key = ordering.to_owned();
        let new_state = Arc::new(cached_state);

        let mut cache_lock = self.cache.write();
        cache_lock.inner_cache.insert(partial_key, new_state);
    }

    /// Retrieves statistics about the cache usage.
    ///
    /// # Returns
    ///
    /// A tuple containing:
    /// - Number of full hits (complete ordering found in cache).
    /// - Number of partial hits (partial prefix found in cache).
    /// - Total number of order simulations saved.
    /// - Total number of order simulations requested.
    /// - Total number of cache requests.
    /// - Rate of full hits (%).
    /// - Rate of partial hits (%).
    /// - Cache efficiency (%).
    #[allow(dead_code)]
    pub fn stats(&self) -> (usize, usize, usize, usize, usize, f64, f64, f64) {
        let full_hits = self.full_hits.load(Ordering::Relaxed);
        let cache_requests = self.cache_requests.load(Ordering::Relaxed);
        let partial_hits = self.partial_hits.load(Ordering::Relaxed);
        let total_simulations_saved = self
            .number_of_order_simulations_saved
            .load(Ordering::Relaxed);
        let total_simulations_requested = self
            .number_of_order_simulations_requested
            .load(Ordering::Relaxed);

        let rate_of_full_hits = if total_simulations_requested > 0 {
            (full_hits as f64 / total_simulations_requested as f64) * 100.0
        } else {
            0.0
        };
        let rate_of_partial_hits = if total_simulations_requested > 0 {
            (partial_hits as f64 / total_simulations_requested as f64) * 100.0
        } else {
            0.0
        };
        let rate_of_cache_efficiency = if total_simulations_requested > 0 {
            (total_simulations_saved as f64 / total_simulations_requested as f64) * 100.0
        } else {
            0.0
        };

        (
            full_hits,
            partial_hits,
            total_simulations_saved,
            total_simulations_requested,
            cache_requests,
            rate_of_full_hits,
            rate_of_partial_hits,
            rate_of_cache_efficiency,
        )
    }
}

impl Default for SharedSimulationCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};
    use reth_primitives::revm_primitives::AccountInfo;
    use revm::db::{states::StorageSlot, AccountStatus, BundleAccount};
    use std::collections::HashMap;

    struct TestDataGenerator {
        last_used_id: u64,
    }

    impl TestDataGenerator {
        fn new() -> Self {
            TestDataGenerator { last_used_id: 0 }
        }

        fn create_order_id(&mut self) -> OrderId {
            self.last_used_id += 1;
            let mut bytes = [0u8; 32];
            bytes[31] = self.last_used_id as u8;
            OrderId::Tx(B256::from(bytes))
        }

        fn create_cached_simulation_state(&mut self) -> CachedSimulationState {
            let mut cached_reads = CachedReads::default();
            let mut storage = HashMap::new();
            storage.insert(U256::from(self.last_used_id), U256::from(self.last_used_id));
            cached_reads.insert_account(Address::random(), AccountInfo::default(), storage);

            let mut storage_bundle_account: HashMap<U256, StorageSlot> = HashMap::new();
            let storage_slot = StorageSlot::new_changed(
                U256::from(self.last_used_id),
                U256::from(self.last_used_id + 1),
            );
            storage_bundle_account.insert(U256::from(self.last_used_id), storage_slot);

            let mut bundle_state = BundleState::default();
            let account = BundleAccount::new(
                Some(AccountInfo::default()),
                Some(AccountInfo::default()),
                storage_bundle_account,
                AccountStatus::Changed,
            );
            bundle_state.state.insert(Address::random(), account);

            CachedSimulationState {
                cached_reads,
                bundle_state,
                total_profit: U256::from(self.last_used_id),
                per_order_profits: vec![(self.create_order_id(), U256::from(10))],
            }
        }
    }

    /// This test verifies that we can store a cached simulation state for a specific ordering
    /// and then retrieve it correctly. It checks if the retrieved state matches the stored state,
    /// including BundleState and CachedReads, and if the cached index is correct.
    #[test]
    fn test_store_and_retrieve_cached_state() {
        let cache = SharedSimulationCache::new();
        let mut gen = TestDataGenerator::new();

        let ordering = vec![
            gen.create_order_id(),
            gen.create_order_id(),
            gen.create_order_id(),
        ];
        let state = gen.create_cached_simulation_state();

        cache.store_cached_state(&ordering, state.clone());

        let (retrieved_state, cached_index) = cache.get_cached_state(&ordering);
        assert_eq!(cached_index, 3);
        assert!(retrieved_state.is_some());
        let retrieved_state = retrieved_state.unwrap();
        assert_eq!(retrieved_state.total_profit, state.total_profit);
        assert_eq!(retrieved_state.bundle_state, state.bundle_state);
    }

    // test that we get a full hit when we request the same ordering again but after storing more
    // states
    #[test]
    fn test_full_hit_after_storing_more_states() {
        let cache = SharedSimulationCache::new();
        let mut gen = TestDataGenerator::new();

        let ordering = vec![
            gen.create_order_id(),
            gen.create_order_id(),
            gen.create_order_id(),
        ];
        let state = gen.create_cached_simulation_state();

        cache.store_cached_state(&ordering, state.clone());

        let ordering2 = vec![
            gen.create_order_id(),
            gen.create_order_id(),
            gen.create_order_id(),
            gen.create_order_id(),
        ];
        let state2 = gen.create_cached_simulation_state();

        cache.store_cached_state(&ordering2, state2.clone());

        let (retrieved_state, cached_index) = cache.get_cached_state(&ordering);
        assert_eq!(cached_index, 3);
        assert!(retrieved_state.is_some());
        let retrieved_state = retrieved_state.unwrap();
        assert_eq!(retrieved_state.total_profit, state.total_profit);
    }

    /// This test checks the partial hit functionality of the cache. It stores a state for a
    /// partial ordering, then attempts to retrieve a state for a longer ordering that includes
    /// the cached partial ordering. It verifies that we get a partial hit, the correct
    /// cached index, and that the BundleState matches the stored partial state.
    #[test]
    fn test_partial_hit() {
        let cache = SharedSimulationCache::new();
        let mut gen = TestDataGenerator::new();

        let ordering = vec![gen.create_order_id(), gen.create_order_id()];
        let state = gen.create_cached_simulation_state();

        cache.store_cached_state(&ordering, state.clone());

        let extended_ordering = vec![ordering[0], ordering[1], gen.create_order_id()];
        let (retrieved_state, cached_index) = cache.get_cached_state(&extended_ordering);
        assert_eq!(cached_index, 2);
        assert!(retrieved_state.is_some());
        let retrieved_state = retrieved_state.unwrap();
        assert_eq!(retrieved_state.bundle_state, state.bundle_state);
    }

    /// This test ensures that the cache correctly handles a miss scenario. It attempts to
    /// retrieve a state for an ordering that hasn't been cached and verifies that we get
    /// a cache miss (no retrieved state and a cached index of 0).
    #[test]
    fn test_cache_miss() {
        let cache = SharedSimulationCache::new();
        let mut gen = TestDataGenerator::new();

        let ordering = vec![
            gen.create_order_id(),
            gen.create_order_id(),
            gen.create_order_id(),
        ];

        let (retrieved_state, cached_index) = cache.get_cached_state(&ordering);
        assert_eq!(cached_index, 0);
        assert!(retrieved_state.is_none());

        // store a state for a different ordering
        let ordering2 = vec![gen.create_order_id(), gen.create_order_id()];
        cache.store_cached_state(&ordering2, gen.create_cached_simulation_state());

        // try to retrieve the original ordering
        let (retrieved_state, cached_index) = cache.get_cached_state(&ordering);

        // we still get a cache miss
        assert_eq!(cached_index, 0);
        assert!(retrieved_state.is_none());
    }
}
