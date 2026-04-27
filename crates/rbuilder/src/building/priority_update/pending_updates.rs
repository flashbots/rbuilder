use ahash::{HashMap, HashSet};
use alloy_primitives::{Address, B256, U256};
use parking_lot::{ArcMutexGuard, RawMutex};
use rbuilder_primitives::{evm_inspector::SlotKey, OrderId};
use reth_errors::ProviderError;
use revm::{
    bytecode::Bytecode, database::states::PlainStorageChangeset, state::AccountInfo, Database,
};
use std::sync::Arc;

use super::PriorityUpdatePool;

#[derive(Debug, Default, Clone)]
pub struct PendingUpdates {
    orders: HashMap<OrderId, Vec<PlainStorageChangeset>>,
    merged: HashMap<(Address, U256), (U256, OrderId)>,
}

impl PendingUpdates {
    pub fn new() -> Self {
        Self {
            orders: HashMap::default(),
            merged: HashMap::default(),
        }
    }

    /// Returns the ids of evicted orders that conflicted with the new one.
    pub fn add_new_simulated_update(
        &mut self,
        order_id: OrderId,
        changeset: Vec<PlainStorageChangeset>,
    ) -> Vec<OrderId> {
        let conflicting: HashSet<OrderId> = changeset
            .iter()
            .flat_map(|s| s.storage.iter().map(|(key, _)| (s.address, *key)))
            .filter_map(|slot| self.merged.get(&slot).map(|(_, id)| *id))
            .collect();

        let evicted: Vec<OrderId> = conflicting.into_iter().collect();
        for id in &evicted {
            self.remove_order(id);
        }

        for storage in &changeset {
            for (key, value) in &storage.storage {
                self.merged
                    .insert((storage.address, *key), (*value, order_id));
            }
        }
        self.orders.insert(order_id, changeset);

        evicted
    }

    pub fn remove_order(&mut self, order_id: &OrderId) {
        if let Some(changeset) = self.orders.remove(order_id) {
            for storage in &changeset {
                for (key, _) in &storage.storage {
                    self.merged.remove(&(storage.address, *key));
                }
            }
        }
    }

    pub fn storage_lookup(&self, address: Address, key: U256) -> Option<U256> {
        self.merged.get(&(address, key)).map(|(value, _)| *value)
    }

    /// OrderId that currently owns the given slot, if any.
    pub fn order_for_slot(&self, slot: &SlotKey) -> Option<OrderId> {
        let key: U256 = slot.key.into();
        self.merged.get(&(slot.address, key)).map(|(_, id)| *id)
    }
}

/// [`Database`] wrapper that overlays storage reads with the pending priority
/// updates from a [`PriorityUpdatePool`].
///
/// Holds an [`ArcMutexGuard`] on the caller's pool for the lifetime of the
/// wrapper (and any clones).
#[derive(Clone)]
pub struct PendingStateDb<DB> {
    pool: Arc<ArcMutexGuard<RawMutex, PriorityUpdatePool>>,
    inner: DB,
}

impl<DB: std::fmt::Debug> std::fmt::Debug for PendingStateDb<DB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingStateDb")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

pub fn wrap_with_pending_state<DB>(
    pool: ArcMutexGuard<RawMutex, PriorityUpdatePool>,
    inner: DB,
) -> PendingStateDb<DB> {
    PendingStateDb {
        pool: Arc::new(pool),
        inner,
    }
}

impl<DB> Database for PendingStateDb<DB>
where
    DB: Database<Error = ProviderError>,
{
    type Error = ProviderError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.inner.basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.inner.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(value) = self
            .pool
            .pending_update_state()
            .storage_lookup(address, index)
        {
            return Ok(value);
        }
        self.inner.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash(number)
    }
}
