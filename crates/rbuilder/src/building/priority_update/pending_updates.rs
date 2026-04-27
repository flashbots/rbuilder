use ahash::{HashMap, HashSet};
use alloy_primitives::{Address, B256, U256};
use rbuilder_primitives::{evm_inspector::SlotKey, OrderId};
use reth_errors::ProviderError;
use revm::{
    bytecode::Bytecode, database::states::PlainStorageChangeset, state::AccountInfo, Database,
};

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

    /// Combined lookup that also returns the OrderId currently owning the slot.
    pub fn storage_lookup_with_owner(
        &self,
        address: Address,
        key: U256,
    ) -> Option<(U256, OrderId)> {
        self.merged.get(&(address, key)).copied()
    }

    /// OrderId that currently owns the given slot, if any.
    pub fn order_for_slot(&self, slot: &SlotKey) -> Option<OrderId> {
        let key: U256 = slot.key.into();
        self.merged.get(&(slot.address, key)).map(|(_, id)| *id)
    }
}

/// [`Database`] wrapper that overlays storage reads with a snapshot of pending
/// priority updates and records which PU orders' slots were read.
#[derive(Clone, Debug)]
pub struct PendingStateDb<'a, DB> {
    pending: &'a PendingUpdates,
    inner: DB,
    used_pu_slots: HashMap<OrderId, SlotKey>,
}

impl<'a, DB> PendingStateDb<'a, DB> {
    pub fn new(pending: &'a PendingUpdates, inner: DB) -> Self {
        Self {
            pending,
            inner,
            used_pu_slots: HashMap::default(),
        }
    }

    pub fn into_used_pu_slots(self) -> Vec<SlotKey> {
        self.used_pu_slots.into_values().collect()
    }
}

impl<DB> Database for PendingStateDb<'_, DB>
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
        if let Some((value, order_id)) = self.pending.storage_lookup_with_owner(address, index) {
            self.used_pu_slots
                .entry(order_id)
                .or_insert_with(|| SlotKey {
                    address,
                    key: index.into(),
                });
            return Ok(value);
        }
        self.inner.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash(number)
    }
}
