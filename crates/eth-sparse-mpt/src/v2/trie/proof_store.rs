use alloy_rlp::Decodable;
use parking_lot::{lock_api::RwLockReadGuard, RawRwLock, RwLock};
use std::sync::Arc;

use alloy_trie::nodes::TrieNode as AlloyTrieNode;
use arrayvec::ArrayVec;
use dashmap::DashMap;
use nybbles::Nibbles;
use rustc_hash::FxBuildHasher;

#[derive(Debug, Clone)]
pub enum ProofNode {
    Leaf { key: usize, value: usize },
    Extension { key: usize, child: usize },
    Branch { children: [Option<usize>; 16] },
    Empty,
}

#[derive(Debug, Clone, Default)]
pub struct ProofStore {
    keys: Arc<RwLock<Vec<Nibbles>>>,
    values: Arc<RwLock<Vec<Box<[u8]>>>>,
    rlp_ptrs: Arc<RwLock<Vec<ArrayVec<u8, 33>>>>,

    pub proofs: Arc<DashMap<Nibbles, Vec<(Nibbles, ProofNode)>, FxBuildHasher>>,
}

impl ProofStore {
    fn add_rlp_ptr(&self, data: ArrayVec<u8, 33>) -> usize {
        let mut arr = self.rlp_ptrs.write();
        let idx = arr.len();
        arr.push(data);
        idx
    }

    fn add_key(&self, data: Nibbles) -> usize {
        let mut arr = self.keys.write();
        let idx = arr.len();
        arr.push(data);
        idx
    }

    fn add_value(&self, data: Vec<u8>) -> usize {
        let mut arr = self.values.write();
        let idx = arr.len();
        arr.push(data.into());
        idx
    }

    pub fn has_proof(&self, key: &Nibbles) -> bool {
        self.proofs.contains_key(key)
    }

    pub fn add_proof<P: AsRef<[u8]>>(
        &self,
        key: Nibbles,
        proof: Vec<(Nibbles, P)>,
    ) -> Result<(), alloy_rlp::Error> {
        if self.proofs.contains_key(&key) {
            return Ok(());
        }

        let mut parsed_proof: Vec<(Nibbles, ProofNode)> = Vec::with_capacity(proof.len());

        for (path, encoded_node) in proof {
            let alloy_trie_node = AlloyTrieNode::decode(&mut encoded_node.as_ref())?;
            let decoded_node = match alloy_trie_node {
                AlloyTrieNode::Branch(alloy_node) => {
                    let mut children: [Option<usize>; 16] = Default::default();
                    let mut stack_iter = alloy_node.stack.into_iter();
                    for index in 0..16 {
                        if alloy_node.state_mask.is_bit_set(index) {
                            let rlp_ptr: ArrayVec<u8, 33> = stack_iter
                                .next()
                                .expect("stack must be the same size as mask")
                                .as_slice()
                                .try_into()
                                .unwrap();
                            children[index as usize] = Some(self.add_rlp_ptr(rlp_ptr));
                        }
                    }
                    ProofNode::Branch { children }
                }
                AlloyTrieNode::Extension(node) => ProofNode::Extension {
                    key: self.add_key(node.key),
                    child: self.add_rlp_ptr(node.child.as_slice().try_into().unwrap()),
                },
                AlloyTrieNode::Leaf(node) => ProofNode::Leaf {
                    key: self.add_key(node.key),
                    value: self.add_value(node.value),
                },
                AlloyTrieNode::EmptyRoot => ProofNode::Empty,
            };
            parsed_proof.push((path, decoded_node));
        }

        self.proofs.insert(key, parsed_proof);

        Ok(())
    }

    // panics if ptr is not stored in this proof store
    pub fn rlp_ptrs(&self) -> RwLockReadGuard<'_, RawRwLock, Vec<ArrayVec<u8, 33>>> {
        self.rlp_ptrs.read()
    }

    pub fn keys_guard(&self) -> RwLockReadGuard<'_, RawRwLock, Vec<Nibbles>> {
        self.keys.read()
    }

    pub fn values_guard(&self) -> RwLockReadGuard<'_, RawRwLock, Vec<Box<[u8]>>> {
        self.values.read()
    }
}
