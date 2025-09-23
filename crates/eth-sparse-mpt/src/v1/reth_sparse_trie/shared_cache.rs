use std::sync::{Arc, RwLock};

use super::{change_set::ETHTrieChangeSet, hash::EthSparseTries, trie_fetcher::MultiProof};
use crate::{
    utils::HashMap,
    v1::sparse_mpt::{AddNodeError, FixedTrie},
};
use alloy_primitives::{Bytes, B256};
use nybbles::Nibbles;

/// SparseTrieSharedCache is a storage for fetched parts of the ethereum tries
/// It should be created once for each parent block and can be shared with a different threads.
#[derive(Debug, Clone, Default)]
pub struct SparseTrieSharedCache {
    internal: Arc<RwLock<RethSparseTrieShareCacheInternal>>,
}

impl SparseTrieSharedCache {
    pub fn new_with_parent_state_root(parent_root_hash: B256) -> Self {
        let mut internal = RethSparseTrieShareCacheInternal::default();
        internal.account_trie.expected_root_hash = parent_root_hash;
        Self {
            internal: Arc::new(RwLock::new(internal)),
        }
    }
}

pub struct StorageTrieInfo {}

#[derive(Default, Debug)]
pub struct MissingNodes {
    pub account_trie_nodes: Vec<Nibbles>,
    pub storage_trie_nodes: HashMap<Bytes, Vec<Nibbles>>,
}

impl MissingNodes {
    pub fn is_empty(&self) -> bool {
        self.account_trie_nodes.is_empty() && self.storage_trie_nodes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.account_trie_nodes.len()
            + self
                .storage_trie_nodes
                .values()
                .map(|n| n.len())
                .sum::<usize>()
    }
}

impl SparseTrieSharedCache {
    pub fn gather_tries_for_changes(
        &self,
        change_set: &ETHTrieChangeSet,
    ) -> Result<EthSparseTries, MissingNodes> {
        let internal = self.internal.read().unwrap();
        internal.gather_tries_for_changes(change_set)
    }

    pub fn update_cache_with_fetched_nodes(
        &self,
        multiproof: MultiProof,
    ) -> Result<(), AddNodeError> {
        let mut internal = self.internal.write().unwrap();
        internal.update_cache_with_fetched_nodes(multiproof)
    }

    pub fn deep_clone(&self) -> Self {
        let internal = self.clone_inner();
        Self {
            internal: Arc::new(RwLock::new(internal)),
        }
    }

    pub fn clone_inner(&self) -> RethSparseTrieShareCacheInternal {
        self.internal.read().unwrap().clone()
    }
}

#[derive(Debug, Clone, Default)]
pub struct RethSparseTrieShareCacheInternal {
    pub account_trie: FixedTrie,
    pub storage_tries: HashMap<Bytes, FixedTrie>,
}

impl RethSparseTrieShareCacheInternal {
    pub fn gather_tries_for_changes(
        &self,
        change_set: &ETHTrieChangeSet,
    ) -> Result<EthSparseTries, MissingNodes> {
        let mut missing_nodes = MissingNodes::default();
        let mut tries = EthSparseTries::default();

        match self.account_trie.gather_subtrie(
            &change_set.account_trie_updates,
            &change_set.account_trie_deletes,
        ) {
            Ok(account_trie) => {
                tries.account_trie = account_trie;
            }
            Err(missing_account_trie_nodes) => {
                missing_nodes.account_trie_nodes = missing_account_trie_nodes;
            }
        }

        for acc_idx in 0..change_set.account_trie_updates.len() {
            let account = change_set.account_trie_updates[acc_idx].clone();
            let updates = &change_set.storage_trie_updated_keys[acc_idx];
            let deletes = &change_set.storage_trie_deleted_keys[acc_idx];
            let storage_trie = match self.storage_tries.get(&account) {
                Some(trie) => trie,
                None => {
                    let mut nodes = Vec::with_capacity(updates.len() + deletes.len());
                    for path in updates.iter().chain(deletes) {
                        let path = Nibbles::unpack(path);
                        nodes.push(path);
                    }
                    if nodes.is_empty() {
                        nodes.push(Nibbles::new());
                    }
                    missing_nodes.storage_trie_nodes.insert(account, nodes);
                    continue;
                }
            };
            match storage_trie.gather_subtrie(updates, deletes) {
                Ok(storage_trie) => {
                    tries.storage_tries.insert(account, storage_trie);
                }
                Err(missing_storage_trie_nodes) => {
                    missing_nodes
                        .storage_trie_nodes
                        .insert(account, missing_storage_trie_nodes);
                }
            }
        }

        if missing_nodes.is_empty() {
            Ok(tries)
        } else {
            Err(missing_nodes)
        }
    }

    pub fn update_cache_with_fetched_nodes(
        &mut self,
        multiproof: MultiProof,
    ) -> Result<(), AddNodeError> {
        let mut nodes: Vec<_> = multiproof.account_subtree.into_iter().collect();
        nodes.sort_by_key(|(p, _)| p.clone());
        self.account_trie.add_nodes(&nodes)?;
        for (account, storge_proofs) in multiproof.storages {
            let mut nodes: Vec<_> = storge_proofs.subtree.into_iter().collect();
            nodes.sort_by_key(|(p, _)| p.clone());
            let account = Bytes::copy_from_slice(account.as_slice());
            let storage_trie = self.storage_tries.entry(account).or_default();
            storage_trie.add_nodes(&nodes)?;
        }
        Ok(())
    }
}
