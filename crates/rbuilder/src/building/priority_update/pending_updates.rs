use ahash::{HashMap, HashSet};
use alloy_primitives::{Address, B256, U256};
use rbuilder_primitives::{evm_inspector::SlotKey, OrderId};
use reth_errors::ProviderError;
use revm::{
    bytecode::Bytecode, database::states::PlainStorageChangeset, state::AccountInfo, Database,
};

use super::used_priority_update_tracer::{
    UsedPriorityStateStorageTracker, UsedPriorityStateTracer,
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
///
/// PU-owned slots are registered with the paired
/// [`UsedPriorityStateStorageTracker`] on first hit and recorded against the
/// currently-executing EVM call frame, so the [`UsedPriorityStateTracer`] can
/// later report whether those reads survived (the surrounding call stack
/// committed) or were inside a reverted subcall.
#[derive(Clone, Debug)]
pub struct PendingStateDb<'a, DB> {
    pending: &'a PendingUpdates,
    tracker: UsedPriorityStateStorageTracker,
    db: DB,
    used_pu_slots: HashMap<OrderId, SlotKey>,
}

impl<DB> PendingStateDb<'_, DB> {
    pub fn into_used_pu_slots(self) -> Vec<SlotKey> {
        self.used_pu_slots.into_values().collect()
    }
}

/// Build a [`PendingStateDb`] alongside a [`UsedPriorityStateTracer`] that
/// share state. Hand the tracer to revm as the EVM inspector and the
/// `PendingStateDb` as the database; after execution call
/// [`UsedPriorityStateTracer::classification`] to learn which PU slots were
/// really used vs. read inside a reverted call.
pub fn new_pending_state_with_tracer<'a, DB>(
    pending: &'a PendingUpdates,
    db: DB,
) -> (PendingStateDb<'a, DB>, UsedPriorityStateTracer) {
    let tracer = UsedPriorityStateTracer::new(std::iter::empty());
    let tracker = tracer.storage_tracker();
    (
        PendingStateDb {
            pending,
            tracker,
            db,
            used_pu_slots: HashMap::default(),
        },
        tracer,
    )
}

impl<DB> Database for PendingStateDb<'_, DB>
where
    DB: Database<Error = ProviderError>,
{
    type Error = ProviderError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.db.basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.db.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some((value, order_id)) = self.pending.storage_lookup_with_owner(address, index) {
            let slot = SlotKey {
                address,
                key: index.into(),
            };
            self.used_pu_slots
                .entry(order_id)
                .or_insert_with(|| slot.clone());
            self.tracker.track_slot(slot.clone());
            self.tracker.record_storage_read(slot);
            return Ok(value);
        }
        self.db.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash(number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        building::priority_update::used_priority_update_tracer::UsedStorageSlotStatus,
        utils::test_utils::{addr, hash, order_id},
    };
    use revm::database::states::PlainStorageChangeset;

    /// Minimal `Database` returning all-zero state — exercise overlay/tracking
    /// behaviour without spinning up a real provider.
    #[derive(Clone, Debug, Default)]
    struct ZeroDb;

    impl Database for ZeroDb {
        type Error = ProviderError;

        fn basic(&mut self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(None)
        }
        fn code_by_hash(&mut self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(Bytecode::default())
        }
        fn storage(&mut self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
            Ok(U256::ZERO)
        }
        fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    fn pu_with_order(order: OrderId, slots: &[(u64, u64, u64)]) -> PendingUpdates {
        let mut pending = PendingUpdates::new();
        let mut by_addr: HashMap<Address, Vec<(U256, U256)>> = HashMap::default();
        for (addr_id, key_id, value) in slots {
            by_addr
                .entry(addr(*addr_id))
                .or_default()
                .push((U256::from(*key_id), U256::from(*value)));
        }
        let changeset: Vec<PlainStorageChangeset> = by_addr
            .into_iter()
            .map(|(address, storage)| PlainStorageChangeset {
                address,
                wipe_storage: false,
                storage,
            })
            .collect();
        pending.add_new_simulated_update(order, changeset);
        pending
    }

    /// Drives the integrated `PendingStateDb` + tracer pair.
    struct Tester<'a> {
        db: PendingStateDb<'a, ZeroDb>,
        tracer: UsedPriorityStateTracer,
    }

    impl<'a> Tester<'a> {
        fn new(pending: &'a PendingUpdates) -> Self {
            let (db, tracer) = new_pending_state_with_tracer(pending, ZeroDb);
            Self { db, tracer }
        }

        fn enter(&mut self) {
            self.tracer.test_enter_frame();
        }

        fn exit(&mut self, ok: bool) {
            self.tracer.test_leave_frame(ok);
        }

        fn read(&mut self, addr_id: u64, key_id: u64) -> U256 {
            self.db
                .storage(addr(addr_id), U256::from(key_id))
                .expect("zero db never errors")
        }

        fn classify(&self, addr_id: u64, key_id: u64) -> Option<UsedStorageSlotStatus> {
            let key = SlotKey {
                address: addr(addr_id),
                key: hash(key_id),
            };
            self.tracer.classification().get(&key).copied()
        }

        fn into_used_pu_slots(self) -> Vec<SlotKey> {
            self.db.into_used_pu_slots()
        }
    }

    #[test]
    fn pu_slot_read_in_committed_root_classifies_as_read() {
        let order = order_id(1);
        let pending = pu_with_order(order, &[(1, 1, 42)]);
        let mut t = Tester::new(&pending);

        t.enter();
        let v = t.read(1, 1);
        t.exit(true);

        assert_eq!(v, U256::from(42));
        assert_eq!(t.classify(1, 1), Some(UsedStorageSlotStatus::Read));
        let used = t.into_used_pu_slots();
        assert_eq!(used.len(), 1);
        assert_eq!(used[0].address, addr(1));
    }

    #[test]
    fn pu_slot_read_only_in_reverted_subcall_is_revert() {
        let order = order_id(1);
        let pending = pu_with_order(order, &[(1, 1, 42)]);
        let mut t = Tester::new(&pending);

        t.enter(); // root
        t.enter(); // sub
        t.read(1, 1);
        t.exit(false); // sub reverts
        t.exit(true); // root commits

        assert_eq!(t.classify(1, 1), Some(UsedStorageSlotStatus::ReadReverted));
        // existing behaviour: used_pu_slots tracks the touched slot regardless
        // — callers can filter by classification to drop reverted reads.
        let used = t.into_used_pu_slots();
        assert_eq!(used.len(), 1);
    }

    #[test]
    fn pu_slot_read_in_revert_then_at_root_is_read() {
        let order = order_id(1);
        let pending = pu_with_order(order, &[(1, 1, 42)]);
        let mut t = Tester::new(&pending);

        t.enter(); // root
        t.enter(); // sub
        t.read(1, 1);
        t.exit(false); // sub reverts
        t.read(1, 1); // re-read at root
        t.exit(true);

        assert_eq!(t.classify(1, 1), Some(UsedStorageSlotStatus::Read));
    }

    #[test]
    fn non_pu_slot_is_not_tracked() {
        let order = order_id(1);
        let pending = pu_with_order(order, &[(1, 1, 42)]); // only (1,1) is PU-owned
        let mut t = Tester::new(&pending);

        t.enter();
        let v = t.read(2, 3); // address/key not in pending
        t.exit(true);

        assert_eq!(v, U256::ZERO); // came from ZeroDb passthrough
        assert!(t.classify(2, 3).is_none());
        assert!(t.into_used_pu_slots().is_empty());
    }

    #[test]
    fn slots_from_multiple_orders_classified_independently() {
        let order_a = order_id(1);
        let order_b = order_id(2);
        let mut pending = PendingUpdates::new();
        pending.add_new_simulated_update(
            order_a,
            vec![PlainStorageChangeset {
                address: addr(1),
                wipe_storage: false,
                storage: vec![(U256::from(1), U256::from(10))],
            }],
        );
        pending.add_new_simulated_update(
            order_b,
            vec![PlainStorageChangeset {
                address: addr(2),
                wipe_storage: false,
                storage: vec![(U256::from(2), U256::from(20))],
            }],
        );
        let mut t = Tester::new(&pending);

        t.enter(); // root
        let v_a = t.read(1, 1); // order_a's slot — committed at root
        t.enter(); // sub
        let v_b = t.read(2, 2); // order_b's slot — inside reverting sub
        t.exit(false);
        t.exit(true);

        assert_eq!(v_a, U256::from(10));
        assert_eq!(v_b, U256::from(20));
        assert_eq!(t.classify(1, 1), Some(UsedStorageSlotStatus::Read));
        assert_eq!(t.classify(2, 2), Some(UsedStorageSlotStatus::ReadReverted));
        assert_eq!(t.into_used_pu_slots().len(), 2);
    }
}
