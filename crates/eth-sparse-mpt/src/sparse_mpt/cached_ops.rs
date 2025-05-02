use std::collections::hash_map::Entry;

use alloy_primitives::{map::foldhash::HashSet, Bytes};
use serde::{Deserialize, Serialize};

use super::{DiffTrie, DiffTrieNode, DiffTrieNodeKind};
use crate::utils::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
enum TrieOp {
    Delete,
    Insert(Bytes, Bytes),
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DiffTrieBreadcrumbs {
    ops: Vec<TrieOp>,
    // node ptr -> op
    modified_node: HashMap<u64, usize>,
    nodes_to_skip: HashSet<u64>,
}

impl DiffTrieBreadcrumbs {
    pub fn begin_insert(&mut self, key: &Bytes, value: &Bytes) {
        self.ops.push(TrieOp::Insert(key.clone(), value.clone()));
    }

    pub fn begin_delete(&mut self) {
        self.ops.push(TrieOp::Delete);
    }

    pub fn mark_node_as_modified(&mut self, node_ptr: u64, old_node: &DiffTrieNode) {
        if self.nodes_to_skip.contains(&node_ptr) {
            return;
        }
        let entry = match self.modified_node.entry(node_ptr) {
            Entry::Occupied(occupied_entry) => {
                self.nodes_to_skip.insert(node_ptr);
                occupied_entry.remove();
                return;
            }
            Entry::Vacant(vacant_entry) => vacant_entry,
        };
        // we only store changes to nodes that are result of modifying fixed trie node
        let skip_node = match &old_node.kind {
            DiffTrieNodeKind::Leaf(node) => node.fixed.is_none(),
            DiffTrieNodeKind::Extension(node) => node.fixed.is_none(),
            DiffTrieNodeKind::Branch(node) => node.fixed.is_none(),
            DiffTrieNodeKind::Null => true,
        };
        if skip_node {
            self.nodes_to_skip.insert(node_ptr);
            return;
        }
        entry.insert(self.ops.len() - 1);
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TrieChangesCache {
    pub cache: HashMap<(Bytes, Bytes), Vec<(u64, DiffTrieNode)>>,
}

impl TrieChangesCache {
    pub fn update_from_trie_bredcrumbs(
        &mut self,
        breadcrumbs: &DiffTrieBreadcrumbs,
        hashed_trie: &DiffTrie,
    ) {
        for (node_ptr, op) in &breadcrumbs.modified_node {
            let key = match breadcrumbs.ops.get(*op).expect("op not found") {
                TrieOp::Insert(key, value) => (key.clone(), value.clone()),
                TrieOp::Delete => {
                    continue;
                }
            };
            let entry = self.cache.entry(key).or_default();
            if entry.iter().any(|(ptr, _)| ptr == node_ptr) {
                continue;
            }
            let node = hashed_trie
                .nodes
                .get(node_ptr)
                .expect("diff trie node not found");
            assert!(
                node.rlp_pointer.is_some(),
                "diff trie must be cached to use TrieChangesCache"
            );
            entry.push((*node_ptr, node.clone()));
        }
    }

    pub fn has_cached_nodes_for_insert(&self, key: &Bytes, value: &Bytes) -> bool {
        self.cache.contains_key(&(key.clone(), value.clone()))
    }

    pub fn get_cached_node_for_insert(
        &self,
        key: &Bytes,
        value: &Bytes,
        node_ptr: u64,
    ) -> Option<DiffTrieNode> {
        self.cache.get(&(key.clone(), value.clone())).and_then(|v| {
            v.iter()
                .find(|(ptr, _)| *ptr == node_ptr)
                .map(|(_, node)| node.clone())
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{sparse_mpt::FixedTrie, test_utils::reference_trie_hash};

    use super::*;

    fn convert_input_to_bytes(input: &[(Vec<u8>, Vec<u8>)]) -> Vec<(Bytes, Bytes)> {
        input
            .iter()
            .map(|(k, v)| (k.clone().into(), v.clone().into()))
            .collect()
    }

    #[test]
    fn test_simple_node_caching() {
        // prepare initial state
        let mut fixed_trie = {
            let mut diff_trie = DiffTrie::new_empty();
            let first_inserts =
                convert_input_to_bytes(&[(vec![0x1], vec![0xa]), (vec![0x2], vec![0xb])]);
            for (key, value) in first_inserts {
                diff_trie.insert(key.clone(), value.clone()).unwrap();
            }
            diff_trie.root_hash().unwrap();
            FixedTrie::from_hashed_diff_trie_test(&diff_trie)
        };

        // fill cache
        let first_inserts =
            convert_input_to_bytes(&[(vec![0x1], vec![0xa1]), (vec![0x2], vec![0xb1])]);
        let (mut diff_trie, _) = fixed_trie
            .gather_subtrie_for_changes(&first_inserts, &[])
            .expect("1st gather fail");
        for (key, value) in first_inserts {
            diff_trie.insert(key, value).expect("1st insert fail");
        }
        diff_trie.root_hash().expect("1st hash fail");
        fixed_trie
            .changes_cache
            .update_from_trie_bredcrumbs(&diff_trie.breadcrumbs, &diff_trie);

        println!("first fill done");

        let first_inserts = convert_input_to_bytes(&[
            (vec![0x1], vec![0xa1]), // this should be cached
            (vec![0x2], vec![0xb2]),
        ]);
        let reference_hash = reference_trie_hash(&first_inserts);

        let (mut diff_trie, cached_inserts) = fixed_trie
            .gather_subtrie_for_changes(&first_inserts, &[])
            .expect("2 gather fail");
        dbg!(&cached_inserts);
        assert_eq!(cached_inserts.len(), 1);
        for (key, value) in first_inserts.clone() {
            if cached_inserts.contains(&key) {
                continue;
            }
            diff_trie.insert(key, value).expect("2 insert fail");
        }
        let computed_hash = diff_trie.root_hash().expect("2 hash fail");
        assert_eq!(reference_hash, computed_hash);
    }
}
