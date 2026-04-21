use ahash::{HashMap, HashSet};
use alloy_primitives::{Address, BlockNumber, Bytes, StorageKey, StorageValue, B256, U256};
use rbuilder_primitives::OrderId;
use reth_errors::ProviderResult;
use reth_primitives::{Account, Bytecode};
use reth_provider::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, StateProofProvider,
    StateProvider, StateRootProvider, StorageRootProvider,
};
use reth_trie::{
    updates::TrieUpdates, AccountProof, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
};
use revm::database::{states::PlainStorageChangeset, BundleState};
use std::sync::Arc;

pub struct PendingUpdates {
    orders: HashMap<OrderId, Vec<PlainStorageChangeset>>,
    merged: HashMap<(Address, U256), (U256, OrderId)>,
}

pub struct PendingState<'a> {
    pub storage: &'a HashMap<(Address, U256), (U256, OrderId)>,
}

impl PendingUpdates {
    pub fn new() -> Self {
        Self {
            orders: HashMap::default(),
            merged: HashMap::default(),
        }
    }

    pub fn add_new_simulated_update(
        &mut self,
        order_id: OrderId,
        changeset: Vec<PlainStorageChangeset>,
    ) {
        let conflicting: HashSet<OrderId> = changeset
            .iter()
            .flat_map(|s| s.storage.iter().map(|(key, _)| (s.address, *key)))
            .filter_map(|slot| self.merged.get(&slot).map(|(_, id)| *id))
            .collect();

        for id in conflicting {
            self.remove_order(&id);
        }

        for storage in &changeset {
            for (key, value) in &storage.storage {
                self.merged
                    .insert((storage.address, *key), (*value, order_id));
            }
        }
        self.orders.insert(order_id, changeset);
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

    pub fn get_current_pending_state(&self) -> PendingState<'_> {
        PendingState {
            storage: &self.merged,
        }
    }
}

impl Default for PendingUpdates {
    fn default() -> Self {
        Self::new()
    }
}

struct PendingStateProvider {
    storage: HashMap<(Address, U256), (U256, OrderId)>,
    parent: Arc<dyn StateProvider>,
}

pub fn wrap_with_pending_state(
    pending: PendingState<'_>,
    parent: Arc<dyn StateProvider>,
) -> Arc<dyn StateProvider> {
    Arc::new(PendingStateProvider {
        storage: pending.storage.clone(),
        parent,
    })
}

impl StateProvider for PendingStateProvider {
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        let key: U256 = storage_key.into();
        if let Some((value, _)) = self.storage.get(&(account, key)) {
            return Ok(Some(*value));
        }
        self.parent.storage(account, storage_key)
    }
}

impl BytecodeReader for PendingStateProvider {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        self.parent.bytecode_by_hash(code_hash)
    }
}

impl AccountReader for PendingStateProvider {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        self.parent.basic_account(address)
    }
}

impl BlockHashReader for PendingStateProvider {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.parent.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.parent.canonical_hashes_range(start, end)
    }
}

impl StateRootProvider for PendingStateProvider {
    fn state_root(&self, hashed_state: HashedPostState) -> ProviderResult<B256> {
        self.parent.state_root(hashed_state)
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        self.parent.state_root_from_nodes(input)
    }

    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.parent.state_root_with_updates(hashed_state)
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.parent.state_root_from_nodes_with_updates(input)
    }
}

impl StorageRootProvider for PendingStateProvider {
    fn storage_root(
        &self,
        address: Address,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        self.parent.storage_root(address, hashed_storage)
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        self.parent.storage_proof(address, slot, hashed_storage)
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        self.parent
            .storage_multiproof(address, slots, hashed_storage)
    }
}

impl StateProofProvider for PendingStateProvider {
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        self.parent.proof(input, address, slots)
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        self.parent.multiproof(input, targets)
    }

    fn witness(&self, input: TrieInput, target: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        self.parent.witness(input, target)
    }
}

impl HashedPostStateProvider for PendingStateProvider {
    fn hashed_post_state(&self, bundle_state: &BundleState) -> HashedPostState {
        self.parent.hashed_post_state(bundle_state)
    }
}
