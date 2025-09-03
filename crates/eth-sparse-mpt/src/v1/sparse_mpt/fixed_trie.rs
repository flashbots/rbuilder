use crate::utils::{fast_hash, hash_map_with_capacity, HashMap};
use alloy_primitives::{keccak256, Bytes, B256};
use alloy_rlp::Decodable;
use alloy_trie::nodes::{
    BranchNode as AlloyBranchNode, ExtensionNode as AlloyExtensionNode, LeafNode as AlloyLeafNode,
    TrieNode as AlloyTrieNode,
};
use nybbles::Nibbles;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, Seq};
use smallvec::SmallVec;
use std::{cmp::max, sync::Arc};

use crate::utils::strip_first_nibble_mut;

use super::{
    get_new_ptr, DiffBranchNode, DiffChildPtr, DiffExtensionNode, DiffLeafNode, DiffTrie,
    DiffTrieNode, DiffTrieNodeKind, NodeCursor,
};

const NULL_NODE_BYTES: Bytes = Bytes::from_static(&[0x80]);
const HEAD_NODE_PATH: Nibbles = Nibbles::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FixedTrieNode {
    Leaf(Arc<FixedLeafNode>),
    Extension {
        node: Arc<FixedExtensionNode>,
        child_ptr: Option<u64>,
    },
    Branch {
        node: Arc<FixedBranchNode>,
        child_ptrs: Vec<(u8, u64)>,
    },
    Null,
}

#[derive(Debug, thiserror::Error)]
pub enum AddNodeError {
    #[error("rlp error: {0:?}")]
    Rlp(#[from] alloy_rlp::Error),
    /// Its possible when input is not sorted or when it has gaps
    /// parent must be added before children
    #[error("Invalid input")]
    InvalidInput,
    /// This happens when proofs added are not from the same trie.
    #[error("Inconsistent proofs added")]
    InconsistentProofs,
}

impl FixedTrieNode {
    fn create_diff_node(&self) -> DiffTrieNode {
        let kind = match self {
            FixedTrieNode::Leaf(leaf) => DiffTrieNodeKind::Leaf(DiffLeafNode {
                fixed: Some(Arc::clone(leaf)),
                changed_key: None,
                changed_value: None,
            }),
            FixedTrieNode::Extension { node, .. } => {
                DiffTrieNodeKind::Extension(DiffExtensionNode {
                    fixed: Some(Arc::clone(node)),
                    changed_key: None,
                    child: DiffChildPtr {
                        rlp_pointer: Some(node.child.clone()),
                        ptr: None,
                    },
                })
            }
            FixedTrieNode::Branch { node, .. } => DiffTrieNodeKind::Branch(DiffBranchNode {
                fixed: Some(Arc::clone(node)),
                changed_children: SmallVec::new(),
                aux_bits: node.child_mask,
            }),
            FixedTrieNode::Null => DiffTrieNodeKind::Null,
        };
        DiffTrieNode {
            kind,
            rlp_pointer: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixedLeafNode {
    pub key: Nibbles,
    pub value: Bytes,
}

impl From<AlloyLeafNode> for FixedLeafNode {
    fn from(alloy_leaf_node: AlloyLeafNode) -> Self {
        Self {
            key: alloy_leaf_node.key,
            value: alloy_leaf_node.value.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixedBranchNode {
    pub children: Box<[Option<Bytes>; 16]>,
    pub child_mask: u16,
}

impl From<AlloyBranchNode> for FixedBranchNode {
    fn from(alloy_node: AlloyBranchNode) -> Self {
        const ARRAY_REPEAT_VALUE: Option<Bytes> = None;
        let mut children = Box::new([ARRAY_REPEAT_VALUE; 16]);
        let mut stack_iter = alloy_node.stack.into_iter();
        let mut child_mask = 0u16;
        for index in 0..16 {
            if alloy_node.state_mask.is_bit_set(index) {
                let rlp_data = stack_iter
                    .next()
                    .expect("stack must be the same size as mask");
                // @Pending: Eval replacing Bytes for ArrayVec to avoid the copy or dig deeper into low-level.
                children[index as usize] = Some(Bytes::copy_from_slice(rlp_data.as_ref()));
                child_mask |= 1 << index
            }
        }
        Self {
            children,
            child_mask,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixedExtensionNode {
    pub key: Nibbles,
    pub child: Bytes,
}

impl From<AlloyExtensionNode> for FixedExtensionNode {
    fn from(alloy_extension_node: AlloyExtensionNode) -> Self {
        Self {
            key: alloy_extension_node.key,
            child: Bytes::copy_from_slice(alloy_extension_node.child.as_ref()),
        }
    }
}

#[serde_as]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FixedTrie {
    /// if set to non-zero we will verify proofs for consistency
    #[serde(default)]
    pub expected_root_hash: B256,
    #[serde_as(as = "Seq<(_, _)>")]
    pub nodes: HashMap<u64, FixedTrieNode>,
    pub head: u64,
    pub ptrs: u64,
    // used to verify proof consistency storsed fast_hash(path), fast_hash(node_from_proof)
    pub nodes_inserted: HashMap<u64, u64>,
    // used for preallocations, wrong value will not influence correctness
    pub height: usize,
}

impl FixedTrie {
    /// Create fixed trie from a diff trie.
    /// used for tests
    /// Only works for diff trie created from scratch and hashed.
    pub fn from_hashed_diff_trie_test(diff_trie: &DiffTrie) -> Self {
        let mut result = Self::default();
        result.head = diff_trie.head;
        result.ptrs = diff_trie.ptrs;

        for (ptr, node) in &diff_trie.nodes {
            let fixed_node = match &node.kind {
                DiffTrieNodeKind::Leaf(leaf) => FixedTrieNode::Leaf(Arc::new(FixedLeafNode {
                    key: leaf.key().clone(),
                    value: leaf.value().clone(),
                })),
                DiffTrieNodeKind::Extension(ext) => FixedTrieNode::Extension {
                    node: Arc::new(FixedExtensionNode {
                        key: ext.key().clone(),
                        child: ext
                            .child
                            .rlp_pointer
                            .clone()
                            .expect("diff trie must be hashed"),
                    }),
                    child_ptr: Some(ext.child.ptr.expect("all children must be in diff trie")),
                },
                DiffTrieNodeKind::Branch(branch) => {
                    const ARRAY_REPEAT_VALUE: Option<Bytes> = None;
                    let mut children = Box::new([ARRAY_REPEAT_VALUE; 16]);
                    let mut child_ptrs = Vec::new();
                    let mut child_mask = 0u16;
                    for n in 0..16u8 {
                        if branch.has_child(n) {
                            let child = branch
                                .get_diff_child(n)
                                .expect("all children must be in diff trie");
                            children[n as usize] =
                                Some(child.rlp_pointer.clone().expect("diff trie must be hashed"));
                            child_ptrs.push((n, child.ptr.expect("diff trie must be complete")));
                            child_mask |= 1 << n;
                        }
                    }
                    FixedTrieNode::Branch {
                        node: Arc::new(FixedBranchNode {
                            children,
                            child_mask,
                        }),
                        child_ptrs,
                    }
                }
                DiffTrieNodeKind::Null => FixedTrieNode::Null,
            };
            result.nodes.insert(*ptr, fixed_node);
        }

        result
    }

    /// nodes must be sorted by key
    /// nodes must be empty if and only if trie is empty
    pub fn add_nodes(&mut self, nodes: &[(Nibbles, Bytes)]) -> Result<(), AddNodeError> {
        // this is not a critical error
        debug_assert!(nodes.iter().is_sorted_by_key(|(path, _)| path));

        // when adding empty proof we init try to be empty
        if nodes.is_empty() && self.nodes.is_empty() {
            self.nodes.insert(0, FixedTrieNode::Null);
            self.head = 0;
            self.ptrs = 0;
            self.nodes_inserted
                .insert(fast_hash(&HEAD_NODE_PATH), fast_hash(&NULL_NODE_BYTES));
        }

        for (path, node) in nodes {
            let path_hash = fast_hash(&path);
            let node_hash = fast_hash(&node);

            if let Some(inserted_node_hash) = self.nodes_inserted.get(&path_hash) {
                if *inserted_node_hash != node_hash {
                    return Err(AddNodeError::InconsistentProofs);
                }
                continue;
            } else if path == &HEAD_NODE_PATH && !self.expected_root_hash.is_zero() {
                // here we verify the first proof that we insert and compare the head node hash to the root hash provided from outside
                let proof_root_hash = keccak256(node);
                if proof_root_hash != self.expected_root_hash {
                    return Err(AddNodeError::InconsistentProofs);
                }
            }

            let alloy_trie_node = AlloyTrieNode::decode(&mut node.as_ref())?;
            let fixed_trie_node = match alloy_trie_node {
                AlloyTrieNode::Branch(node) => FixedTrieNode::Branch {
                    node: Arc::new(node.into()),
                    child_ptrs: Vec::new(),
                },
                AlloyTrieNode::Extension(node) => FixedTrieNode::Extension {
                    node: Arc::new(node.into()),
                    child_ptr: None,
                },
                AlloyTrieNode::Leaf(node) => FixedTrieNode::Leaf(Arc::new(node.into())),
                AlloyTrieNode::EmptyRoot => FixedTrieNode::Null,
            };

            // here we find parent to link with this new node
            let mut current_path = Nibbles::new();
            let mut path_left = path.clone();
            let mut current_node = self.head;

            let mut parent: Option<u64> = None;
            let mut parent_child_idx: Option<u8> = None;

            // we start from 1 because we are looking for parent in the loop below and we will terminate on the parent
            let mut height = 1;
            loop {
                // parent was wound
                if path_left.is_empty() {
                    break;
                }
                let node = match self.nodes.get(&current_node) {
                    Some(node) => node,
                    None => return Err(AddNodeError::InvalidInput),
                };
                height += 1;
                match node {
                    FixedTrieNode::Extension { node, child_ptr } => {
                        if path_left.starts_with(&node.key) {
                            parent = Some(current_node);
                            parent_child_idx = None;

                            let len = node.key.len();
                            current_path.extend_from_slice_unchecked(&path_left[..len]);
                            path_left.as_mut_vec_unchecked().drain(..len);

                            if path_left.is_empty() {
                                break;
                            }

                            current_node = child_ptr.ok_or(AddNodeError::InvalidInput)?;
                            continue;
                        }
                        return Err(AddNodeError::InvalidInput);
                    }
                    FixedTrieNode::Branch { child_ptrs, .. } => {
                        if path_left.is_empty() {
                            return Err(AddNodeError::InvalidInput);
                        }
                        let nibble = strip_first_nibble_mut(&mut path_left);

                        parent = Some(current_node);
                        parent_child_idx = Some(nibble);

                        if path_left.is_empty() {
                            break;
                        }

                        current_path.push_unchecked(nibble);
                        current_node =
                            get_child_ptr(child_ptrs, nibble).ok_or(AddNodeError::InvalidInput)?;
                    }
                    FixedTrieNode::Null | FixedTrieNode::Leaf(_) => {
                        return Err(AddNodeError::InvalidInput)
                    }
                }
            }
            self.height = max(self.height, height);

            self.nodes_inserted.insert(path_hash, node_hash);
            let ptr = get_new_ptr(&mut self.ptrs);
            self.nodes.insert(ptr, fixed_trie_node);

            if let Some(parent) = parent {
                let parent_node = self.nodes.get_mut(&parent).unwrap();
                match parent_node {
                    FixedTrieNode::Extension { child_ptr, .. } => {
                        *child_ptr = Some(ptr);
                    }
                    FixedTrieNode::Branch { child_ptrs, .. } => {
                        let child_nibble = parent_child_idx.unwrap();
                        assert!(get_child_ptr(child_ptrs, child_nibble).is_none());
                        child_ptrs.push((child_nibble, ptr));
                    }
                    FixedTrieNode::Null | FixedTrieNode::Leaf(_) => unreachable!(),
                }
            } else {
                assert_eq!(self.nodes.len(), 1);
                assert_eq!(self.head, 0);
                self.head = ptr;
            }
        }
        Ok(())
    }

    pub fn gather_subtrie(
        &self,
        changed_keys: &[Bytes],
        deleted_keys: &[Bytes],
    ) -> Result<DiffTrie, Vec<Nibbles>> {
        let mut missing_nodes = Vec::new();
        let mut result = DiffTrie::default();
        result.nodes =
            hash_map_with_capacity(self.height * (changed_keys.len() + deleted_keys.len()));
        result.head = self.head;
        result.ptrs = self.ptrs;

        let additional_change = if changed_keys.is_empty() && deleted_keys.is_empty() {
            Some((Bytes::new(), false))
        } else {
            None
        };
        let additional_change_iter = additional_change.as_ref().map(|(p, b)| (p, *b));

        let iter = changed_keys
            .iter()
            .zip(std::iter::repeat(false))
            .chain(deleted_keys.iter().zip(std::iter::repeat(true)))
            .chain(additional_change_iter);

        for (changed_key, delete) in iter {
            let mut c = NodeCursor::new(Nibbles::unpack(changed_key), self.head);
            loop {
                let node = match self.nodes.get(&c.current_node) {
                    Some(node) => node,
                    None => {
                        missing_nodes.push(Nibbles::unpack(changed_key));
                        break;
                    }
                };
                let diff_node = result
                    .nodes
                    .entry(c.current_node)
                    .or_insert_with(|| node.create_diff_node());
                match (node, &mut diff_node.kind) {
                    (FixedTrieNode::Null, DiffTrieNodeKind::Null) => {
                        // this is empty trie, we have everything to return
                        return Ok(result);
                    }
                    (FixedTrieNode::Leaf(_), DiffTrieNodeKind::Leaf(_)) => {
                        break;
                    }
                    (
                        FixedTrieNode::Extension { child_ptr, .. },
                        DiffTrieNodeKind::Extension(extension),
                    ) => {
                        if c.path_left.starts_with(extension.key()) {
                            extension.child.ptr = *child_ptr;
                            // go deeper
                            c.step_into_extension(extension);
                            continue;
                        }
                        break;
                    }
                    (
                        FixedTrieNode::Branch {
                            child_ptrs,
                            node: fixed_branch,
                        },
                        DiffTrieNodeKind::Branch(branch),
                    ) => {
                        if c.path_left.is_empty() {
                            break;
                        }
                        let nibble = c.next_nibble();
                        if fixed_branch.children[nibble as usize].is_none() {
                            break;
                        }
                        let fixed_child_ptr = get_child_ptr(child_ptrs, nibble);
                        branch.insert_diff_child(
                            nibble,
                            DiffChildPtr {
                                rlp_pointer: None,
                                ptr: fixed_child_ptr,
                            },
                        );
                        c.step_into_branch(branch);
                        if delete {
                            branch.aux_bits &= !(1 << nibble);
                            if branch.aux_bits.count_ones() == 1 {
                                let orphan_nibble = branch.aux_bits.trailing_zeros() as u8;
                                if branch.get_diff_child(orphan_nibble).is_none() {
                                    // that means that might be orphan was not added to the diff trie
                                    if let Some(orphan_ptr) =
                                        get_child_ptr(child_ptrs, orphan_nibble)
                                    {
                                        branch.insert_diff_child(
                                            orphan_nibble,
                                            DiffChildPtr {
                                                rlp_pointer: None,
                                                ptr: Some(orphan_ptr),
                                            },
                                        );
                                        let orphan_node =
                                            self.nodes.get(&orphan_ptr).expect("must be in trie");
                                        result
                                            .nodes
                                            .insert(orphan_ptr, orphan_node.create_diff_node());
                                    } else {
                                        // orphan node is missing
                                        // we stepped into child above so the path is the path of current child and orphan child differs
                                        // only in last nibble
                                        let mut path = c.current_path.clone();
                                        path.as_mut_vec_unchecked()
                                            .last_mut()
                                            .map(|n| *n = orphan_nibble)
                                            .unwrap();
                                        missing_nodes.push(path);
                                    }
                                }
                            }
                        }
                    }
                    _ => unreachable!(),
                }
            }
        }

        if missing_nodes.is_empty() {
            Ok(result)
        } else {
            Err(missing_nodes)
        }
    }
}

fn get_child_ptr(child_ptrs: &[(u8, u64)], nibble: u8) -> Option<u64> {
    child_ptrs
        .iter()
        .find_map(|(c, ptr)| if c == &nibble { Some(*ptr) } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{get_test_change_set, get_test_multiproofs};

    #[test]
    fn test_insert_and_gather_account_trie() {
        let account_proof = {
            let multiproof = get_test_multiproofs();
            let mut account_proof: Vec<_> = multiproof
                .into_iter()
                .flat_map(|mp| mp.account_subtree.into_iter().collect::<Vec<_>>())
                .collect();
            account_proof.sort_by_key(|(p, _)| p.clone());
            account_proof.dedup_by_key(|(p, _)| p.clone());
            account_proof
        };

        let mut fixed_trie = FixedTrie::default();
        fixed_trie.add_nodes(&account_proof).expect("must add");
        assert_eq!(fixed_trie.nodes.len(), account_proof.len());
        assert_eq!(fixed_trie.nodes_inserted.len(), account_proof.len());

        fixed_trie.add_nodes(&account_proof).expect("must add 2");
        assert_eq!(fixed_trie.nodes.len(), account_proof.len());
        assert_eq!(fixed_trie.nodes_inserted.len(), account_proof.len());

        let change_set = get_test_change_set();
        let mut gather_result = fixed_trie
            .gather_subtrie(
                &change_set.account_trie_updates,
                &change_set.account_trie_deletes,
            )
            .expect("must gather");
        dbg!(gather_result.nodes.len());
        for key in change_set.account_trie_updates {
            gather_result.insert(key.clone(), key).expect("must insert");
        }
        let root_hash = gather_result.clone().root_hash().expect("must hash");
        let root_hash_parallel = gather_result
            .clone()
            .root_hash_parallel()
            .expect("must hash");
        assert_eq!(root_hash, root_hash_parallel);
    }
}
