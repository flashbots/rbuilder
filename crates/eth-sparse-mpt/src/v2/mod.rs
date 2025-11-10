use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use dashmap::DashMap;
use fetch::MissingNodesFetcher;
use nybbles::Nibbles;
use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use reth_provider::{providers::ConsistentDbView, BlockReader, DatabaseProviderFactory};
use reth_trie::TrieAccount;
use revm::{
    database::{BundleAccount, BundleState},
    state::AccountInfo,
};
use rustc_hash::FxBuildHasher;
use std::{ops::Range, sync::Arc, time::Instant};
use trie::{
    proof_store::ProofStore, DeletionError, InsertValue, NodeNotFound, ProofError, ProofWithValue,
    Trie,
};

use crate::{
    utils::{HashMap, HashSet},
    ChangedAccountData, SparseTrieError, SparseTrieMetrics,
};

pub mod fetch;
pub mod trie;

const PARALLEL_HASHING_STORAGE_NODES: bool = true;

#[derive(Debug, Default, Clone)]
pub struct SharedCacheV2 {
    pub account_trie: ProofStore,
    pub storage_tries: Arc<DashMap<B256, ProofStore, FxBuildHasher>>,
    pub last_block_hash: B256,
}

impl SharedCacheV2 {
    pub fn account_proof_store_hashed_address(&self, hashed_address: &B256) -> ProofStore {
        if let Some(store) = self.storage_tries.get(hashed_address) {
            store.value().clone()
        } else {
            self.storage_tries
                .entry(*hashed_address)
                .or_default()
                .clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StorageTrieStatus {
    InsertsNotProcessed,
    InsertsProcessed,
    Hashed,
}

impl StorageTrieStatus {
    fn needs_processing(&self) -> bool {
        match self {
            StorageTrieStatus::InsertsNotProcessed => true,
            StorageTrieStatus::InsertsProcessed => false,
            StorageTrieStatus::Hashed => false,
        }
    }
}

#[derive(Debug, Default)]
pub struct RootHashCalculator {
    storage: DashMap<Address, Arc<Mutex<StorageCalculator>>, FxBuildHasher>,
    changed_account: Arc<RwLock<Vec<(Address, StorageTrieStatus)>>>,

    account_trie: AccountTrieCalculator,

    shared_cache: SharedCacheV2,

    // if set, only changes for these accounts will be incrementally applied
    incremental_account_change: HashSet<Address>,
}

impl Clone for RootHashCalculator {
    fn clone(&self) -> Self {
        Self {
            storage: self
                .storage
                .iter()
                .map(|entry| {
                    (
                        *entry.key(),
                        Arc::new(Mutex::new(entry.value().lock().clone())),
                    )
                })
                .collect(),
            changed_account: Arc::new(RwLock::new(self.changed_account.read().clone())),
            account_trie: self.account_trie.clone(),
            shared_cache: self.shared_cache.clone(),
            incremental_account_change: self.incremental_account_change.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct AppliedAccountOp {
    inserted_value: Option<TrieAccount>,
    revert_key: Nibbles,
    revert_value: Option<Range<usize>>,
}

#[derive(Debug, Clone, Default)]
struct AccountTrieCalculator {
    trie: Trie,

    applied_account_ops_current_iteration: HashMap<Address, AppliedAccountOp>,

    revert_account_ops: Vec<AppliedAccountOp>,
    revert_account_ops_done: Vec<bool>,

    insert_keys: Vec<Nibbles>,
    insert_values: Vec<Vec<u8>>,
    insert_account_keys: Vec<Address>,
    insert_account_values: Vec<TrieAccount>,
    insert_ok: Vec<bool>,

    delete_keys: Vec<Nibbles>,
    delete_account_keys: Vec<Address>,
    delete_ok: Vec<bool>,

    proof_keys: Vec<Nibbles>,
    proof_account_keys: Vec<Address>,
    proof_ok: Vec<bool>,
    proof_result: Vec<(Address, Vec<(Nibbles, Vec<u8>)>)>,

    missing_nodes: Vec<Nibbles>,
    missing_nodes_requested: Vec<Nibbles>,

    applied_account_ops_previous_iteration: HashMap<Address, AppliedAccountOp>,
}

impl AccountTrieCalculator {
    fn clear(&mut self) {
        self.applied_account_ops_current_iteration.clear();
        self.revert_account_ops.clear();
        self.revert_account_ops_done.clear();

        self.insert_keys.clear();
        self.insert_account_keys.clear();
        self.insert_account_values.clear();
        self.insert_values.clear();
        self.insert_ok.clear();
        self.delete_keys.clear();
        self.delete_account_keys.clear();
        self.delete_ok.clear();
        self.missing_nodes.clear();

        self.proof_keys.clear();
        self.proof_account_keys.clear();
        self.proof_ok.clear();
        self.proof_result.clear();
        // self.trie.clear();
    }
}

#[derive(Debug, Clone)]
struct AppliedStorageOp {
    inserted_value: U256,
    revert_key: Nibbles,
    revert_value: Option<Range<usize>>,
}

#[derive(Debug, Clone, Default)]
struct StorageCalculator {
    hashed_address: B256,
    unpacked_hashed_address: Nibbles,

    account_info: Option<AccountInfo>,

    applied_storage_ops_current_iteration: HashMap<U256, AppliedStorageOp>,

    revert_storage_ops: Vec<AppliedStorageOp>,
    revert_storage_ops_done: Vec<bool>,

    insert_keys: Vec<Nibbles>,
    insert_values: Vec<Vec<u8>>,
    insert_storage_key: Vec<U256>,
    insert_storage_value: Vec<U256>,
    insert_ok: Vec<bool>,

    delete_keys: Vec<Nibbles>,
    delete_storage_key: Vec<U256>,
    delete_ok: Vec<bool>,

    missing_nodes: Vec<Nibbles>,
    missing_nodes_requested: Vec<Nibbles>,

    trie: Trie,
    applied_storage_ops_previous_iteration: HashMap<U256, AppliedStorageOp>,

    proof_store: ProofStore,
    hash: B256,
}

impl StorageCalculator {
    fn new(addres: Address, shared_cache: &SharedCacheV2) -> Self {
        let hashed_address = keccak256(addres);
        let unpacked_hashed_address = Nibbles::unpack(hashed_address.as_slice());
        let proof_store = shared_cache.account_proof_store_hashed_address(&hashed_address);

        StorageCalculator {
            hashed_address,
            unpacked_hashed_address,
            account_info: None,
            revert_storage_ops: Vec::new(),
            revert_storage_ops_done: Vec::new(),
            insert_keys: Vec::new(),
            insert_storage_key: Vec::new(),
            insert_storage_value: Vec::new(),
            insert_values: Vec::new(),
            insert_ok: Vec::new(),
            delete_keys: Vec::new(),
            delete_storage_key: Vec::new(),
            delete_ok: Vec::new(),
            missing_nodes: Vec::new(),
            missing_nodes_requested: Vec::new(),
            trie: Trie::default(),
            applied_storage_ops_previous_iteration: HashMap::default(),
            applied_storage_ops_current_iteration: HashMap::default(),
            hash: B256::ZERO,
            proof_store,
        }
    }
    // it does not clear the trie with currently applied op!
    fn clear(&mut self) {
        self.account_info = None;
        self.applied_storage_ops_current_iteration.clear();
        self.revert_storage_ops.clear();
        self.revert_storage_ops_done.clear();

        self.insert_keys.clear();
        self.insert_values.clear();
        self.insert_storage_key.clear();
        self.insert_storage_value.clear();
        self.insert_ok.clear();

        self.delete_keys.clear();
        self.delete_storage_key.clear();
        self.delete_ok.clear();

        self.missing_nodes.clear();
        self.missing_nodes_requested.clear();
    }
}

pub fn prefetch_proofs<'a, Provider>(
    consistent_db_view: ConsistentDbView<Provider>,
    shared_cache: &SharedCacheV2,
    changed_data: impl Iterator<Item = &'a ChangedAccountData>,
) -> Result<SparseTrieMetrics, SparseTrieError>
where
    Provider: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync,
{
    let mut metrics = SparseTrieMetrics::default();
    let mut fetcher = MissingNodesFetcher::default();

    for data in changed_data {
        let hashed_address = keccak256(data.address.as_slice());
        let account_node = Nibbles::unpack(hashed_address);
        fetcher.add_missing_account_node(account_node);
        for (slot, _) in &data.slots {
            let storage_node = Nibbles::unpack(keccak256(B256::from(*slot)).as_slice());
            fetcher.add_missing_storage_node(&hashed_address, storage_node);
        }
    }
    metrics.fetched_nodes += fetcher.fetch_nodes(shared_cache, &consistent_db_view)?;

    Ok(metrics)
}

impl RootHashCalculator {
    pub fn new(shared_cache: SharedCacheV2) -> Self {
        Self {
            account_trie: AccountTrieCalculator::default(),
            storage: DashMap::default(),
            changed_account: Default::default(),
            shared_cache,
            incremental_account_change: Default::default(),
        }
    }

    fn get_account_storage(&self, address: &Address) -> Arc<Mutex<StorageCalculator>> {
        if let Some(v) = self.storage.get(address) {
            v.value().clone()
        } else {
            self.storage
                .entry(*address)
                .or_insert_with(|| {
                    Arc::new(Mutex::new(StorageCalculator::new(
                        *address,
                        &self.shared_cache,
                    )))
                })
                .value()
                .clone()
        }
    }

    fn prepare_changes_for_one_storage_trie(
        &self,
        address: Address,
        bundle_account: &BundleAccount,
    ) {
        let storage_calc = self.get_account_storage(&address);
        let mut storage_calc = storage_calc.lock();
        let storage_calc = &mut *storage_calc;

        storage_calc.clear();
        storage_calc.account_info = bundle_account.account_info().map(|a| a.without_code());

        if storage_calc.account_info.is_none() {
            // account processed, no need to compute storage hash
            storage_calc.hash = B256::ZERO;
            self.changed_account
                .write()
                .push((address, StorageTrieStatus::Hashed));
            return;
        }

        for (storage_key, storage_value) in &bundle_account.storage {
            if !storage_value.is_changed() {
                continue;
            }

            let storage_value = storage_value.present_value();

            if let Some(applied_op) = storage_calc
                .applied_storage_ops_previous_iteration
                .remove(storage_key)
            {
                if applied_op.inserted_value == storage_value {
                    storage_calc
                        .applied_storage_ops_current_iteration
                        .insert(*storage_key, applied_op);
                    continue;
                } else {
                    storage_calc.revert_storage_ops.push(applied_op);
                    storage_calc.revert_storage_ops_done.push(false);
                }
            }

            let hashed_key = Nibbles::unpack(keccak256(B256::from(*storage_key)).as_slice());
            if !storage_value.is_zero() {
                let value = alloy_rlp::encode(storage_value);
                storage_calc.insert_keys.push(hashed_key);
                storage_calc.insert_values.push(value);
                storage_calc.insert_storage_key.push(*storage_key);
                storage_calc.insert_storage_value.push(storage_value);
                storage_calc.insert_ok.push(false);
            } else {
                storage_calc.delete_keys.push(hashed_key);
                storage_calc.delete_storage_key.push(*storage_key);
                storage_calc.delete_ok.push(false);
            }
        }

        // revert all applied ops from previous iteration
        let mut applied_storage_ops_previous_iteration =
            std::mem::take(&mut storage_calc.applied_storage_ops_previous_iteration);
        for (_, applied_op) in applied_storage_ops_previous_iteration.drain() {
            storage_calc.revert_storage_ops.push(applied_op);
            storage_calc.revert_storage_ops_done.push(false);
        }
        storage_calc.applied_storage_ops_previous_iteration =
            applied_storage_ops_previous_iteration;

        if storage_calc.delete_keys.is_empty()
            && storage_calc.insert_keys.is_empty()
            && storage_calc.revert_storage_ops.is_empty()
            && !storage_calc.hash.is_zero()
            && !storage_calc.trie.is_uninit()
        {
            std::mem::swap(
                &mut storage_calc.applied_storage_ops_previous_iteration,
                &mut storage_calc.applied_storage_ops_current_iteration,
            );
            self.changed_account
                .write()
                .push((address, StorageTrieStatus::Hashed));
        } else {
            storage_calc.hash = B256::ZERO;
            self.changed_account
                .write()
                .push((address, StorageTrieStatus::InsertsNotProcessed));
        }
    }

    fn prepare_changes_for_storage_trie(&mut self, outcome: &BundleState) -> eyre::Result<()> {
        self.changed_account.write().clear();

        let incremental_change = !self.incremental_account_change.is_empty();

        if incremental_change {
            self.incremental_account_change.iter().for_each(|address| {
                let bundle_account = outcome
                    .account(address)
                    .expect("account with incremental change is not in the BundleState");
                self.prepare_changes_for_one_storage_trie(*address, bundle_account)
            });
        } else {
            outcome
                .state()
                .iter()
                .map(|(a, acc)| (*a, acc))
                .par_bridge()
                .for_each(|(address, bundle_account)| {
                    if bundle_account.status.is_not_modified() {
                        return;
                    }
                    self.prepare_changes_for_one_storage_trie(address, bundle_account)
                });
        }

        Ok(())
    }

    fn do_first_fetch<Provider>(
        &mut self,
        consistent_db_view: &ConsistentDbView<Provider>,
        stats: &mut Stats,
    ) -> Result<(), SparseTrieError>
    where
        Provider: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync,
    {
        stats.start();

        let fetcher = Arc::new(Mutex::new(MissingNodesFetcher::default()));
        self.changed_account
            .read()
            .par_iter()
            .for_each(|(address, _)| {
                let fetcher = fetcher.clone();
                let storage_calc = self.get_account_storage(address);
                let storage_calc = storage_calc.lock();

                if !self
                    .shared_cache
                    .account_trie
                    .has_proof(&storage_calc.unpacked_hashed_address)
                {
                    fetcher
                        .lock()
                        .add_missing_account_node(storage_calc.unpacked_hashed_address.clone());
                }

                if storage_calc.insert_keys.is_empty() && storage_calc.delete_keys.is_empty() {
                    let node = Nibbles::new();
                    if !storage_calc.proof_store.has_proof(&node) {
                        fetcher
                            .lock()
                            .add_missing_storage_node(&storage_calc.hashed_address, node);
                    }
                }

                for node in storage_calc
                    .insert_keys
                    .iter()
                    .chain(storage_calc.delete_keys.iter())
                {
                    if !storage_calc.proof_store.has_proof(node) {
                        fetcher
                            .lock()
                            .add_missing_storage_node(&storage_calc.hashed_address, node.clone());
                    }
                }
            });

        stats.measure_other();

        let mut fetcher = fetcher.lock();
        if !fetcher.is_empty() {
            stats.start_proof_fetch_db();
            let nodes_fetched = fetcher.fetch_nodes(&self.shared_cache, consistent_db_view)?;
            stats.fetched_nodes += nodes_fetched;
            stats.measure_proof_fetch_db_part();
        }

        Ok(())
    }

    // true if no missing nodes
    fn process_storage_tries_update(&self, address: Address) -> eyre::Result<bool> {
        let storage_calc = self.get_account_storage(&address);
        let mut storage_calc = storage_calc.lock();
        let storage_calc = &mut *storage_calc;
        storage_calc.missing_nodes.clear();

        for i in 0..storage_calc.revert_storage_ops_done.len() {
            if storage_calc.revert_storage_ops_done[i] {
                continue;
            }

            let applied_op = &storage_calc.revert_storage_ops[i];

            let missing_node = match applied_op.revert_value.clone() {
                Some(old_value) => {
                    match storage_calc.trie.insert_nibble_key(
                        &applied_op.revert_key,
                        InsertValue::StoredValue(old_value),
                    ) {
                        Ok(_) => None,
                        Err(node_not_found) => Some(node_not_found.0),
                    }
                }
                None => {
                    match storage_calc.trie.delete_nibbles_key(&applied_op.revert_key) {
                        Ok(_) => None,
                        Err(DeletionError::KeyNotFound) => {
                            eyre::bail!("reverting nodes, can't delete key that is not in the trie (storage)");
                        }
                        Err(DeletionError::NodeNotFound(node_not_found)) => Some(node_not_found.0),
                    }
                }
            };
            match missing_node {
                Some(node) => {
                    storage_calc.missing_nodes.push(node);
                }
                None => {
                    storage_calc.revert_storage_ops_done[i] = true;
                }
            }
        }
        // this way we alway process reverts first
        if !storage_calc.missing_nodes.is_empty() {
            return Ok(false);
        }

        for i in 0..storage_calc.insert_ok.len() {
            if storage_calc.insert_ok[i] {
                continue;
            }

            let insertion_result = storage_calc.trie.insert_nibble_key(
                &storage_calc.insert_keys[i],
                InsertValue::Value(&storage_calc.insert_values[i]),
            );

            match insertion_result {
                Ok(revert_value) => {
                    storage_calc.insert_ok[i] = true;
                    storage_calc.applied_storage_ops_current_iteration.insert(
                        storage_calc.insert_storage_key[i],
                        AppliedStorageOp {
                            inserted_value: storage_calc.insert_storage_value[i],
                            revert_key: storage_calc.insert_keys[i].clone(),
                            revert_value,
                        },
                    );
                }
                Err(NodeNotFound(missing_node)) => {
                    storage_calc.missing_nodes.push(missing_node);
                }
            }
        }

        for i in 0..storage_calc.delete_ok.len() {
            if storage_calc.delete_ok[i] {
                continue;
            }
            let deletion_result = storage_calc
                .trie
                .delete_nibbles_key(&storage_calc.delete_keys[i]);
            match deletion_result {
                Ok(revert_value) => {
                    storage_calc.delete_ok[i] = true;
                    storage_calc.applied_storage_ops_current_iteration.insert(
                        storage_calc.delete_storage_key[i],
                        AppliedStorageOp {
                            inserted_value: U256::ZERO,
                            revert_key: storage_calc.delete_keys[i].clone(),
                            revert_value: Some(revert_value),
                        },
                    );
                }
                Err(DeletionError::NodeNotFound(NodeNotFound(missing_node))) => {
                    storage_calc.missing_nodes.push(missing_node);
                }
                Err(DeletionError::KeyNotFound) => {
                    eyre::bail!("Deleting key that is not in the trie");
                }
            }
        }

        if storage_calc.trie.is_uninit() {
            storage_calc.missing_nodes.push(Nibbles::new());
        }
        Ok(storage_calc.missing_nodes.is_empty())
    }

    fn process_storage_tries_updates(&mut self) -> eyre::Result<bool> {
        let all_changed_processed = Arc::new(Mutex::new(true));
        self.changed_account
            .write()
            .par_iter_mut()
            .map(|(address, status)| -> eyre::Result<()> {
                // if account is done, just return
                if !status.needs_processing() {
                    return Ok(());
                }
                if self.process_storage_tries_update(*address)? {
                    *status = StorageTrieStatus::InsertsProcessed;
                } else {
                    *all_changed_processed.lock() = false;
                }
                Ok(())
            })
            .collect::<Result<(), _>>()?;
        let res = *all_changed_processed.lock();
        Ok(res)
    }

    fn fetch_missing_storage_nodes<Provider>(
        &mut self,
        consistent_db_view: &ConsistentDbView<Provider>,
        stats: &mut Stats,
    ) -> Result<(), SparseTrieError>
    where
        Provider: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync,
    {
        let fetcher = Arc::new(Mutex::new(MissingNodesFetcher::default()));

        self.changed_account.read().par_iter().for_each(|(address, status)| {
            if !status.needs_processing() {
                return;
            }
            let storage_calc = self.get_account_storage(address);
            let mut storage_calc = storage_calc.lock();
            let storage_calc = &mut *storage_calc;
            storage_calc.missing_nodes_requested.clear();
            for missing_node in storage_calc.missing_nodes.drain(..) {
                if storage_calc.proof_store.has_proof(&missing_node) {
                    let ok = storage_calc.trie.try_add_proof_from_proof_store(&missing_node, &storage_calc.proof_store).expect("should be able to insert proofs from proof store when they are found (storage trie)");
                    assert!(ok, "proof is not added (storage trie)");
                } else {
                    storage_calc.missing_nodes_requested.push(missing_node.clone());
                    fetcher.lock().add_missing_storage_node(&storage_calc.hashed_address, missing_node);
                }
            }
        });

        let mut fetcher = fetcher.lock();
        if !fetcher.is_empty() {
            stats.start_proof_fetch_db();
            let nodes_fetched = fetcher.fetch_nodes(&self.shared_cache, consistent_db_view)?;
            stats.fetched_nodes += nodes_fetched;
            stats.measure_proof_fetch_db_part();
        }

        self.changed_account.read().par_iter().for_each(|(address, status)| {
            if !status.needs_processing() {
                return;
            }
            let storage_calc = self.get_account_storage(address);
            let mut storage_calc = storage_calc.lock();
            let storage_calc = &mut *storage_calc;
            for missing_node in storage_calc.missing_nodes_requested.drain(..) {
                if storage_calc.proof_store.has_proof(&missing_node) {
                    let ok = storage_calc.trie.try_add_proof_from_proof_store(&missing_node, &storage_calc.proof_store).expect("should be able to insert proofs from proof store when they are found (storage trie)");
                    assert!(ok, "proof is not added (storage trie)");
                } else {
                    panic!("Missing node that was just fetched is not there (storage trie)");
                }
            }
        });
        Ok(())
    }

    fn hash_storage_tries(&mut self) {
        self.changed_account
            .read()
            .par_iter()
            .for_each(|(address, status)| {
                if status == &StorageTrieStatus::Hashed {
                    return;
                }
                assert_eq!(
                    status,
                    &StorageTrieStatus::InsertsProcessed,
                    "storage trie is not updated properly"
                );
                let storage_calc = self.get_account_storage(address);
                let mut storage_calc = storage_calc.lock();
                let storage_calc = &mut *storage_calc;
                let hash = storage_calc
                    .trie
                    .root_hash(PARALLEL_HASHING_STORAGE_NODES, &storage_calc.proof_store)
                    .expect("missing node while hahsing storage trie");
                storage_calc.hash = hash;
                std::mem::swap(
                    &mut storage_calc.applied_storage_ops_previous_iteration,
                    &mut storage_calc.applied_storage_ops_current_iteration,
                );
            });
    }

    fn prepare_changes_account_trie(&mut self, proof_targets: &HashSet<Address>) {
        self.account_trie.clear();

        for (address, _) in &*self.changed_account.read() {
            let storage_calc = self.get_account_storage(address);
            let storage_calc = storage_calc.lock();

            let trie_account = storage_calc.account_info.as_ref().map(|account_info| {
                assert!(!storage_calc.hash.is_zero());
                TrieAccount {
                    nonce: account_info.nonce,
                    balance: account_info.balance,
                    storage_root: storage_calc.hash,
                    code_hash: account_info.code_hash,
                }
            });

            if let Some(applied_op) = self
                .account_trie
                .applied_account_ops_previous_iteration
                .remove(address)
            {
                if applied_op.inserted_value == trie_account {
                    self.account_trie
                        .applied_account_ops_current_iteration
                        .insert(*address, applied_op);
                    continue;
                } else {
                    self.account_trie.revert_account_ops.push(applied_op);
                    self.account_trie.revert_account_ops_done.push(false);
                }
            }

            let key = storage_calc.unpacked_hashed_address.clone();
            if let Some(trie_account) = trie_account {
                let value = alloy_rlp::encode(trie_account);
                self.account_trie.insert_keys.push(key);
                self.account_trie.insert_values.push(value);
                self.account_trie.insert_account_keys.push(*address);
                self.account_trie.insert_account_values.push(trie_account);
                self.account_trie.insert_ok.push(false);
            } else {
                self.account_trie.delete_keys.push(key);
                self.account_trie.delete_account_keys.push(*address);
                self.account_trie.delete_ok.push(false);
            }
        }

        let incremental_change = !self.incremental_account_change.is_empty();

        for (address, applied_op) in self
            .account_trie
            .applied_account_ops_previous_iteration
            .drain()
        {
            if incremental_change && !self.incremental_account_change.contains(&address) {
                self.account_trie
                    .applied_account_ops_current_iteration
                    .insert(address, applied_op);
            } else {
                self.account_trie.revert_account_ops.push(applied_op);
                self.account_trie.revert_account_ops_done.push(false);
            }
        }

        for address in proof_targets {
            let storage_calc = self.get_account_storage(address);
            let storage_calc = storage_calc.lock();
            let key = storage_calc.unpacked_hashed_address.clone();
            self.account_trie.proof_keys.push(key);
            self.account_trie.proof_account_keys.push(*address);
            self.account_trie.proof_ok.push(false);
        }
    }

    fn process_account_tries_update(&mut self) -> eyre::Result<bool> {
        let account_trie = &mut self.account_trie;
        account_trie.missing_nodes.clear();

        for i in 0..account_trie.revert_account_ops_done.len() {
            if account_trie.revert_account_ops_done[i] {
                continue;
            }
            let applied_op = &account_trie.revert_account_ops[i];
            let missing_node = match applied_op.revert_value.clone() {
                Some(old_value) => {
                    match account_trie.trie.insert_nibble_key(
                        &applied_op.revert_key,
                        InsertValue::StoredValue(old_value),
                    ) {
                        Ok(_) => None,
                        Err(node_not_found) => Some(node_not_found.0),
                    }
                }
                None => {
                    match account_trie.trie.delete_nibbles_key(&applied_op.revert_key) {
                        Ok(_) => None,
                        Err(DeletionError::KeyNotFound) => {
                            eyre::bail!("reverting nodes, can't delete key that is not in the trie (accounts)");
                        }
                        Err(DeletionError::NodeNotFound(node_not_found)) => Some(node_not_found.0),
                    }
                }
            };

            match missing_node {
                Some(node) => {
                    account_trie.missing_nodes.push(node);
                }
                None => {
                    account_trie.revert_account_ops_done[i] = true;
                }
            }
        }
        // this way we alway process reverts first
        if !account_trie.missing_nodes.is_empty() {
            return Ok(false);
        }

        for i in 0..account_trie.insert_ok.len() {
            if account_trie.insert_ok[i] {
                continue;
            }
            let insertion_result = account_trie.trie.insert_nibble_key(
                &account_trie.insert_keys[i],
                InsertValue::Value(&account_trie.insert_values[i]),
            );
            match insertion_result {
                Ok(revert_value) => {
                    account_trie.insert_ok[i] = true;
                    account_trie.applied_account_ops_current_iteration.insert(
                        account_trie.insert_account_keys[i],
                        AppliedAccountOp {
                            inserted_value: Some(account_trie.insert_account_values[i]),
                            revert_key: account_trie.insert_keys[i].clone(),
                            revert_value,
                        },
                    );
                }
                Err(NodeNotFound(missing_node)) => {
                    account_trie.missing_nodes.push(missing_node);
                }
            }
        }

        for i in 0..account_trie.delete_ok.len() {
            if account_trie.delete_ok[i] {
                continue;
            }
            let deletion_result = account_trie
                .trie
                .delete_nibbles_key(&account_trie.delete_keys[i]);
            match deletion_result {
                Ok(revert_value) => {
                    account_trie.delete_ok[i] = true;
                    account_trie.applied_account_ops_current_iteration.insert(
                        account_trie.delete_account_keys[i],
                        AppliedAccountOp {
                            inserted_value: None,
                            revert_key: account_trie.delete_keys[i].clone(),
                            revert_value: Some(revert_value),
                        },
                    );
                }
                Err(DeletionError::NodeNotFound(NodeNotFound(missing_node))) => {
                    account_trie.missing_nodes.push(missing_node);
                }
                Err(DeletionError::KeyNotFound) => {
                    eyre::bail!("Deleting key that is not in the trie (account trie)");
                }
            }
        }
        Ok(account_trie.missing_nodes.is_empty())
    }

    fn process_account_trie_proofs(&mut self, shared_cache: &SharedCacheV2) -> eyre::Result<bool> {
        let account_trie = &mut self.account_trie;
        account_trie.missing_nodes.clear();
        for i in 0..account_trie.proof_ok.len() {
            if account_trie.proof_ok[i] {
                continue;
            }
            let proof_result = account_trie
                .trie
                .get_proof_nibbles_key(&account_trie.proof_keys[i], &shared_cache.account_trie);
            match proof_result {
                Ok(ProofWithValue { proof, .. }) => {
                    account_trie.proof_ok[i] = true;
                    account_trie
                        .proof_result
                        .push((account_trie.proof_account_keys[i], proof));
                }
                Err(ProofError::TrieIsDirty) => {
                    eyre::bail!("Trie is not hashed before fetching proofs")
                }
                Err(ProofError::NodeNotFound(NodeNotFound(missing_node))) => {
                    account_trie.missing_nodes.push(missing_node);
                }
            }
        }
        Ok(account_trie.missing_nodes.is_empty())
    }

    fn fetch_missing_account_trie_nodes<Provider>(
        &mut self,
        consistent_db_view: &ConsistentDbView<Provider>,
        stats: &mut Stats,
    ) -> Result<(), SparseTrieError>
    where
        Provider: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync,
    {
        let mut fetcher = MissingNodesFetcher::default();

        let proof_store = &self.shared_cache.account_trie;
        let account_trie = &mut self.account_trie;
        account_trie.missing_nodes_requested.clear();
        for missing_node in account_trie.missing_nodes.drain(..) {
            if proof_store.has_proof(&missing_node) {
                let ok = account_trie.trie.try_add_proof_from_proof_store(&missing_node, proof_store).expect("should be able to insert proofs from proof store when they are found (storage trie)");
                assert!(ok, "proof is not added (storage trie)");
            } else {
                account_trie
                    .missing_nodes_requested
                    .push(missing_node.clone());
                fetcher.add_missing_account_node(missing_node);
            }
        }

        if !fetcher.is_empty() {
            stats.start_proof_fetch_db();
            let nodes_fetched = fetcher.fetch_nodes(&self.shared_cache, consistent_db_view)?;
            stats.fetched_nodes += nodes_fetched;
            stats.measure_proof_fetch_db_part();
        }

        for missing_node in account_trie.missing_nodes_requested.drain(..) {
            if proof_store.has_proof(&missing_node) {
                let ok = account_trie.trie.try_add_proof_from_proof_store(&missing_node, proof_store).expect("should be able to insert proofs from proof store when they are found (account trie)");
                assert!(ok, "proof is not added (account trie)");
            } else {
                panic!("Missing node that was just fetched is not there (account trie)");
            }
        }

        Ok(())
    }

    pub fn hash_account_trie(&mut self, shared_cache: &SharedCacheV2) -> B256 {
        let hash = self
            .account_trie
            .trie
            .root_hash(true, &shared_cache.account_trie)
            .expect("failed to hash account trie");
        std::mem::swap(
            &mut self.account_trie.applied_account_ops_current_iteration,
            &mut self.account_trie.applied_account_ops_previous_iteration,
        );
        hash
    }

    pub fn calculate_root_hash_with_sparse_trie<Provider>(
        &mut self,
        consistent_db_view: ConsistentDbView<Provider>,
        shared_cache: SharedCacheV2,
        outcome: &BundleState,
        incremental_change: &[Address],
        proof_targets: &HashSet<Address>,
    ) -> Result<(B256, HashMap<Address, Vec<Bytes>>, SparseTrieMetrics), SparseTrieError>
    where
        Provider: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync,
    {
        if !incremental_change.is_empty() {
            self.incremental_account_change.extend(incremental_change);
        } else {
            self.incremental_account_change.clear();
        }

        let mut stats = Stats::default();
        stats.start_global();

        self.shared_cache = shared_cache.clone();

        stats.start();
        self.prepare_changes_for_storage_trie(outcome)?;
        stats.measure_prepare(true);
        if self.incremental_account_change.is_empty() {
            self.do_first_fetch(&consistent_db_view, &mut stats)?;
        }

        let mut loop_break = false;
        for _ in 0..10 {
            stats.start();
            let ok = self.process_storage_tries_updates()?;
            stats.measure_insert(true);
            if !ok {
                stats.start();
                self.fetch_missing_storage_nodes(&consistent_db_view, &mut stats)?;
                stats.measure_proof_fetch(true);
                continue;
            }
            stats.start();
            self.hash_storage_tries();
            stats.measure_hash(true);
            loop_break = true;
            break;
        }
        assert!(loop_break, "storage trie are not processed after 10 iters");

        stats.start();
        self.prepare_changes_account_trie(proof_targets);
        stats.measure_prepare(false);

        let mut loop_break = false;
        let mut root_hash = B256::ZERO;
        for _ in 0..10 {
            stats.start();
            let ok = self.process_account_tries_update()?;
            stats.measure_insert(false);
            if !ok {
                stats.start();
                self.fetch_missing_account_trie_nodes(&consistent_db_view, &mut stats)?;
                stats.measure_proof_fetch(false);
                continue;
            }
            stats.start();
            root_hash = self.hash_account_trie(&shared_cache);
            stats.measure_hash(false);
            loop_break = true;
            break;
        }
        assert!(loop_break, "account trie are not processed after 10 iters");

        let mut loop_break = false;
        for _ in 0..10 {
            stats.start();
            let ok = self.process_account_trie_proofs(&shared_cache)?;
            stats.measure_other();
            if !ok {
                stats.start();
                self.fetch_missing_account_trie_nodes(&consistent_db_view, &mut stats)?;
                stats.measure_proof_fetch(false);
                // if we fetched proofs we need to rehash account trie
                stats.start();
                self.account_trie
                    .trie
                    .root_hash(true, &shared_cache.account_trie)
                    .map_err(|err| {
                        eyre::eyre!("failed to hash account trie (account proofs) {err:?}")
                    })?;
                stats.measure_other();
                continue;
            }
            loop_break = true;
            break;
        }
        assert!(
            loop_break,
            "account trie proofs are not processed after 10 iters"
        );

        let mut proofs = HashMap::default();
        for (address, proof) in self.account_trie.proof_result.drain(..) {
            proofs.insert(
                address,
                proof.into_iter().map(|(_, node)| node.into()).collect(),
            );
        }
        for proof_target in proof_targets {
            if !proofs.contains_key(proof_target) {
                return Err(SparseTrieError::Other(eyre::eyre!(
                    "Proof was not fethed correctly"
                )));
            }
        }

        let mut metrics = SparseTrieMetrics::default();
        metrics.fetched_nodes = stats.fetched_nodes;
        stats.finalize_and_print();
        Ok((root_hash, proofs, metrics))
    }
}

#[derive(Debug, Clone, Default)]
struct Stats {
    global_start: Option<Instant>,
    current_start: Option<Instant>,
    proof_fetch_db_start: Option<Instant>,

    prepare_storage: f64,
    prepare_account: f64,

    insert_storage: f64,
    insert_account: f64,

    proof_fetch_storage: f64,
    proof_fetch_account: f64,

    proof_fetch_db_part: f64,

    hash_storage: f64,
    hash_account: f64,

    other: f64,

    fetched_nodes: usize,
}

impl Stats {
    fn start_global(&mut self) {
        self.global_start = Some(Instant::now());
    }

    fn finalize_and_print(self) {
        let elapsed = elapsed_ms(self.global_start.unwrap());
        let elapsed_no_db = elapsed - self.proof_fetch_db_part;

        let Stats {
            prepare_storage,
            prepare_account,
            insert_storage,
            insert_account,
            proof_fetch_storage,
            proof_fetch_account,
            proof_fetch_db_part,
            hash_storage,
            hash_account,
            other,
            fetched_nodes,
            ..
        } = self;

        tracing::trace!(
            elapsed,
            elapsed_no_db,
            prepare_storage,
            prepare_account,
            insert_storage,
            insert_account,
            proof_fetch_storage,
            proof_fetch_account,
            proof_fetch_db_part,
            hash_storage,
            hash_account,
            other,
            fetched_nodes,
            "Root hash"
        );
    }

    fn start(&mut self) {
        self.current_start = Some(Instant::now());
    }

    fn start_proof_fetch_db(&mut self) {
        self.proof_fetch_db_start = Some(Instant::now());
    }

    fn measure_prepare(&mut self, storage: bool) {
        let elapsed = elapsed_ms(self.current_start.unwrap());
        if storage {
            self.prepare_storage += elapsed;
        } else {
            self.prepare_account += elapsed;
        }
    }
    fn measure_insert(&mut self, storage: bool) {
        let elapsed = elapsed_ms(self.current_start.unwrap());
        if storage {
            self.insert_storage += elapsed;
        } else {
            self.insert_account += elapsed;
        }
    }
    fn measure_proof_fetch(&mut self, storage: bool) {
        let elapsed = elapsed_ms(self.current_start.unwrap());
        if storage {
            self.proof_fetch_storage += elapsed;
        } else {
            self.proof_fetch_account += elapsed;
        }
    }
    fn measure_proof_fetch_db_part(&mut self) {
        let elapsed = elapsed_ms(self.proof_fetch_db_start.unwrap());
        self.proof_fetch_db_part += elapsed;
    }
    fn measure_hash(&mut self, storage: bool) {
        let elapsed = elapsed_ms(self.current_start.unwrap());
        if storage {
            self.hash_storage += elapsed;
        } else {
            self.hash_account += elapsed;
        }
    }
    fn measure_other(&mut self) {
        let elapsed = elapsed_ms(self.current_start.unwrap());
        self.other += elapsed;
    }
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_nanos() as f64 / 1_000_000.0
}
