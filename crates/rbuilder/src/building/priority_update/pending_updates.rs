use ahash::{HashMap, HashSet};
use alloy_primitives::{Address, B256, U256};
use parking_lot::{ArcRwLockReadGuard, RawRwLock};
use rbuilder_primitives::OrderId;
use reth_errors::ProviderError;
use revm::{
    bytecode::Bytecode, database::states::PlainStorageChangeset, state::AccountInfo, Database,
};
use std::sync::Arc;

#[derive(Debug)]
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
}

impl Default for PendingUpdates {
    fn default() -> Self {
        Self::new()
    }
}

/// [`Database`] wrapper that overlays storage reads with [`PendingUpdates`].
/// Holds the overlay read lock for the lifetime of the wrapper (and any clones).
#[derive(Clone, Debug)]
pub struct PendingStateDb<DB> {
    pending: Arc<ArcRwLockReadGuard<RawRwLock, PendingUpdates>>,
    inner: DB,
}

pub fn wrap_with_pending_state<DB>(
    pending: ArcRwLockReadGuard<RawRwLock, PendingUpdates>,
    inner: DB,
) -> PendingStateDb<DB> {
    PendingStateDb {
        pending: Arc::new(pending),
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
        if let Some(value) = self.pending.storage_lookup(address, index) {
            return Ok(value);
        }
        self.inner.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash(number)
    }
}
