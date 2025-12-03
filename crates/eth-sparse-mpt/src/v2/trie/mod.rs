use std::ops::Range;

use alloy_primitives::{keccak256, B256};
use alloy_rlp::EMPTY_STRING_CODE;
use arrayvec::ArrayVec;

use nybbles::Nibbles;
use proof_store::{ProofNode, ProofStore};

pub mod proof_store;

#[cfg(test)]
mod tests;

use crate::utils::{encode_branch_node, encode_extension, encode_leaf, mismatch};

#[derive(Debug, Clone, Copy)]
enum NodePtr {
    Local(usize),
    Remote(usize),
}

impl NodePtr {
    #[inline]
    fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    #[inline]
    fn as_local(&self) -> Option<usize> {
        match self {
            Self::Local(idx) => Some(*idx),
            Self::Remote(_) => None,
        }
    }

    #[inline]
    fn expect_local(&self, msg: &str) -> usize {
        self.as_local()
            .unwrap_or_else(|| panic!("eth-sparse-mpt expect local node: {msg}"))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Trie {
    // 3 arrays below are of the same length
    hashed_nodes: Vec<bool>,
    rlp_ptrs_local: Vec<ArrayVec<u8, 33>>,
    nodes: Vec<DiffTrieNode>,

    values: Vec<u8>,
    keys: Vec<u8>,
    branch_node_children: Vec<[Option<NodePtr>; 16]>,

    // scratchpad
    walk_path: Vec<(usize, u8)>, // node index, nibble
}

#[derive(Debug, Clone)]
enum DiffTrieNode {
    Leaf {
        key: Range<usize>,
        value: Range<usize>,
    },
    Extension {
        key: Range<usize>,
        next_node: NodePtr,
    },
    Branch {
        children: usize,
    },
    Null,
}

#[derive(Debug, thiserror::Error)]
pub enum DeletionError {
    #[error("Deletion error: {0:?}")]
    NodeNotFound(#[from] NodeNotFound),
    #[error("Key node not found in the trie")]
    KeyNotFound,
}

#[derive(Debug, thiserror::Error)]
pub enum ProofError {
    #[error("Proof node not found: {0:?}")]
    NodeNotFound(#[from] NodeNotFound),
    #[error("Trie is dirty")]
    TrieIsDirty,
}

#[derive(Debug, thiserror::Error)]
#[error("Node not found")]
pub struct NodeNotFound(pub Nibbles);

impl NodeNotFound {
    fn new(path: &[u8]) -> Self {
        Self(Nibbles::from_nibbles_unchecked(path))
    }
}

#[derive(Debug)]
pub enum InsertValue<'a> {
    Value(&'a [u8]),
    StoredValue(Range<usize>),
}

#[derive(Debug, Clone)]
pub struct ProofWithValue {
    pub proof: Vec<(Nibbles, Vec<u8>)>,
    pub value: Option<Vec<u8>>,
}

impl Trie {
    pub fn is_uninit(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn new_empty() -> Self {
        let mut def = Self::default();
        def.clear_empty();
        def
    }

    pub fn clear_empty(&mut self) {
        self.clear();
        self.push_node(DiffTrieNode::Null);
    }

    fn push_node(&mut self, node: DiffTrieNode) -> NodePtr {
        let idx = self.nodes.len();
        self.nodes.push(node);
        self.hashed_nodes.push(false);
        self.rlp_ptrs_local.push(Default::default());
        NodePtr::Local(idx)
    }

    fn insert_value(&mut self, insert_value: InsertValue<'_>) -> Range<usize> {
        match insert_value {
            InsertValue::Value(slice) => self.copy_value(slice),
            InsertValue::StoredValue(range) => range.clone(),
        }
    }

    fn copy_value(&mut self, value: &[u8]) -> Range<usize> {
        let offset = self.values.len();
        self.values.extend_from_slice(value);
        offset..offset + value.len()
    }

    fn insert_key(&mut self, key: &[u8]) -> Range<usize> {
        let offset = self.keys.len();
        self.keys.extend_from_slice(key);
        offset..offset + key.len()
    }

    fn create_branch_children(&mut self) -> usize {
        let idx = self.branch_node_children.len();
        self.branch_node_children.push(Default::default());
        idx
    }

    pub fn clear(&mut self) {
        self.hashed_nodes.clear();
        self.rlp_ptrs_local.clear();
        self.nodes.clear();

        self.values.clear();
        self.keys.clear();
        self.branch_node_children.clear();
    }

    // return prefix (as part of path_left, stripped nibble of suffix 1, suffix 1 as part of path_left, stripped nibble from suffix2, suffix2 as part of key stored)
    fn extract_prefix_and_suffix<'a>(
        &self,
        path_left: &'a [u8],
        key: Range<usize>,
    ) -> (&'a [u8], u8, &'a [u8], u8, Range<usize>) {
        let p = mismatch(path_left, &self.keys[key.clone()]);
        let prefix = &path_left[..p];
        let n1 = path_left[p];
        let suff1 = &path_left[(p + 1)..];
        let n2 = self.keys[key.start + p];
        let suff2 = (key.start + p + 1)..key.end;
        (prefix, n1, suff1, n2, suff2)
    }

    pub fn insert(
        &mut self,
        key: &[u8],
        insert_value: &[u8],
    ) -> Result<Option<Range<usize>>, NodeNotFound> {
        let n = Nibbles::unpack(key);
        self.insert_nibble_key(&n, InsertValue::Value(insert_value))
    }

    // returns old value
    pub fn insert_nibble_key(
        &mut self,
        nibbles_key: &Nibbles,
        insert_value: InsertValue<'_>,
    ) -> Result<Option<Range<usize>>, NodeNotFound> {
        let ins_key = nibbles_key.as_slice();

        let mut current_node = 0;
        let mut path_walked = 0;

        let mut old_value = None;

        loop {
            let node = self
                .nodes
                .get(current_node)
                .ok_or_else(|| NodeNotFound(nibbles_key.clone()))?;
            self.hashed_nodes[current_node] = false;
            match node {
                DiffTrieNode::Branch { children } => {
                    let children = *children;

                    let n = ins_key[path_walked] as usize;
                    path_walked += 1;
                    if let Some(child_ptr) = self.branch_node_children[children][n] {
                        current_node = child_ptr
                            .as_local()
                            .ok_or_else(|| NodeNotFound(nibbles_key.clone()))?;
                        continue;
                    } else {
                        let new_leaf_key = self.insert_key(&ins_key[path_walked..]);
                        let leaf_value = self.insert_value(insert_value);
                        let leaf_ptr = self.push_node(DiffTrieNode::Leaf {
                            key: new_leaf_key,
                            value: leaf_value,
                        });
                        self.branch_node_children[children][n] = Some(leaf_ptr);
                    }
                }
                DiffTrieNode::Extension { key, next_node } => {
                    let key = key.clone();
                    let next_node = *next_node;

                    if ins_key[path_walked..].starts_with(&self.keys[key.clone()]) {
                        path_walked += key.len();
                        current_node = next_node
                            .as_local()
                            .ok_or_else(|| NodeNotFound(nibbles_key.clone()))?;
                        continue;
                    }

                    let (prefix, n1, suff1, n2, suff2) =
                        self.extract_prefix_and_suffix(&ins_key[path_walked..], key);

                    let has_extension_node = !prefix.is_empty();
                    if has_extension_node {
                        let new_ext_key = self.insert_key(prefix);
                        // next node will be a branch node that we will push below
                        let ext_next_node = NodePtr::Local(self.nodes.len());
                        self.nodes[current_node] = DiffTrieNode::Extension {
                            key: new_ext_key,
                            next_node: ext_next_node,
                        };
                    };
                    let branch_children = self.create_branch_children();
                    let branch_node = DiffTrieNode::Branch {
                        children: branch_children,
                    };
                    if has_extension_node {
                        self.push_node(branch_node);
                    } else {
                        self.nodes[current_node] = branch_node;
                    }

                    let new_leaf_key = self.insert_key(suff1);
                    let new_leaf_value = self.insert_value(insert_value);

                    let new_leaf_ptr = self.push_node(DiffTrieNode::Leaf {
                        key: new_leaf_key,
                        value: new_leaf_value,
                    });

                    let branch_child = if !suff2.is_empty() {
                        self.push_node(DiffTrieNode::Extension {
                            key: suff2,
                            next_node,
                        })
                    } else {
                        next_node
                    };

                    self.branch_node_children[branch_children][n1 as usize] = Some(new_leaf_ptr);
                    self.branch_node_children[branch_children][n2 as usize] = Some(branch_child);
                }
                DiffTrieNode::Leaf { key, value } => {
                    let key = key.clone();
                    let value = value.clone();

                    if self.keys[key.clone()] == ins_key[path_walked..] {
                        // update leaf in place
                        old_value = Some(value.clone());
                        let new_value = self.insert_value(insert_value);
                        self.nodes[current_node] = DiffTrieNode::Leaf {
                            key: key.clone(),
                            value: new_value,
                        };
                        break;
                    }

                    let (prefix, n1, suff1, n2, suff2) =
                        self.extract_prefix_and_suffix(&ins_key[path_walked..], key);

                    let has_extension_node = !prefix.is_empty();
                    if has_extension_node {
                        let new_ext_key = self.insert_key(prefix);
                        // next node will branch node that we will push below
                        let ext_next_node = NodePtr::Local(self.nodes.len());
                        self.nodes[current_node] = DiffTrieNode::Extension {
                            key: new_ext_key,
                            next_node: ext_next_node,
                        };
                    };
                    let branch_children = self.create_branch_children();
                    let branch_node = DiffTrieNode::Branch {
                        children: branch_children,
                    };
                    if has_extension_node {
                        self.push_node(branch_node);
                    } else {
                        self.nodes[current_node] = branch_node;
                    }

                    let first_leaf_key = self.insert_key(suff1);
                    let first_leaf_value = self.insert_value(insert_value);
                    let first_leaf_ptr = self.push_node(DiffTrieNode::Leaf {
                        key: first_leaf_key,
                        value: first_leaf_value,
                    });

                    let second_leaf_key = suff2;
                    let second_leaf_value = value;
                    let second_leaf_ptr = self.push_node(DiffTrieNode::Leaf {
                        key: second_leaf_key,
                        value: second_leaf_value,
                    });

                    self.branch_node_children[branch_children][n1 as usize] = Some(first_leaf_ptr);
                    self.branch_node_children[branch_children][n2 as usize] = Some(second_leaf_ptr);
                }
                DiffTrieNode::Null => {
                    let new_leaf_key = self.insert_key(&ins_key[path_walked..]);
                    let new_leaf_value = self.insert_value(insert_value);
                    self.nodes[current_node] = DiffTrieNode::Leaf {
                        key: new_leaf_key,
                        value: new_leaf_value,
                    };
                }
            }
            break;
        }
        Ok(old_value)
    }

    fn merge_keys(&mut self, key1: Range<usize>, nibble: u8, key2: Range<usize>) -> Range<usize> {
        let new_start = self.keys.len();
        let new_len = self.keys.len() + key1.len() + key2.len() + 1;
        self.keys.resize(new_len, 0);
        self.keys.copy_within(key1.clone(), new_start);
        self.keys[new_start + key1.len()] = nibble;
        self.keys.copy_within(key2, new_start + key1.len() + 1);
        new_start..new_len
    }

    // returns old value
    pub fn delete(&mut self, key: &[u8]) -> Result<Range<usize>, DeletionError> {
        let n = Nibbles::unpack(key);
        self.delete_nibbles_key(&n)
    }

    pub fn delete_nibbles_key(
        &mut self,
        nibbles_key: &Nibbles,
    ) -> Result<Range<usize>, DeletionError> {
        let del_key = nibbles_key.as_slice();

        let mut current_node = 0;
        let mut path_walked = 0;

        self.walk_path.clear();

        let old_value;

        loop {
            let node = self
                .nodes
                .get(current_node)
                .ok_or_else(|| NodeNotFound(nibbles_key.clone()))?;
            self.hashed_nodes[current_node] = false;
            match node {
                DiffTrieNode::Branch { children } => {
                    // deleting from branch, key not found
                    if del_key.len() == path_walked {
                        return Err(DeletionError::KeyNotFound);
                    }

                    let children = *children;

                    let n = del_key[path_walked];
                    self.walk_path.push((current_node, n));
                    path_walked += 1;

                    if let Some(child_ptr) = self.branch_node_children[children][n as usize] {
                        current_node = child_ptr
                            .as_local()
                            .ok_or_else(|| NodeNotFound(nibbles_key.clone()))?;
                        if self.branch_node_children[children]
                            .iter()
                            .filter(|c| c.is_some())
                            .count()
                            == 2
                        {
                            // we will have an orphan, make sure that we we have it in the trie
                            let (orphan_nibble, orphan_ptr) = self.branch_node_children[children]
                                .iter()
                                .enumerate()
                                .find(|(nib, c)| c.is_some() && *nib as u8 != n)
                                .unwrap();
                            let orphan_ptr = orphan_ptr.unwrap();
                            if orphan_ptr.is_remote() {
                                let mut orphan_path = Nibbles::with_capacity(path_walked);
                                orphan_path
                                    .extend_from_slice_unchecked(&del_key[..(path_walked - 1)]);
                                orphan_path.push_unchecked(orphan_nibble as u8);
                                return Err(NodeNotFound(orphan_path).into());
                            }
                        }
                        continue;
                    } else {
                        return Err(DeletionError::KeyNotFound);
                    }
                }
                DiffTrieNode::Extension { key, next_node } => {
                    let key = key.clone();
                    let next_node = *next_node;

                    if del_key[path_walked..].starts_with(&self.keys[key.clone()]) {
                        self.walk_path.push((current_node, 0));
                        path_walked += key.len();
                        current_node = next_node
                            .as_local()
                            .ok_or_else(|| NodeNotFound(nibbles_key.clone()))?;
                        continue;
                    }

                    return Err(DeletionError::KeyNotFound);
                }
                DiffTrieNode::Leaf { key, value } => {
                    if self.keys[key.clone()] == del_key[path_walked..] {
                        old_value = value.clone();
                        self.walk_path.push((current_node, 0));
                        break;
                    }
                    return Err(DeletionError::KeyNotFound);
                }
                DiffTrieNode::Null => {
                    return Err(DeletionError::KeyNotFound);
                }
            }
        }

        #[derive(Debug)]
        enum NodeDeletionResult {
            NodeDeleted,
            NodeUpdated,
            BranchBelowRemovedWithOneChild {
                child_nibble: u8,
                child_ptr: NodePtr,
            },
        }

        let mut deletion_result = NodeDeletionResult::NodeDeleted;

        for (current_node, current_node_child) in self.walk_path.iter().rev() {
            let current_node = *current_node;
            let current_node_child = *current_node_child;
            match deletion_result {
                NodeDeletionResult::NodeDeleted => {
                    match &self.nodes[current_node] {
                        DiffTrieNode::Leaf { .. } => {
                            deletion_result = NodeDeletionResult::NodeDeleted;
                        }
                        DiffTrieNode::Branch { children } => {
                            let children = &mut self.branch_node_children[*children];
                            let children_count = children.iter().filter(|c| c.is_some()).count();
                            match children_count {
                                3.. => {
                                    children[current_node_child as usize] = None;
                                    deletion_result = NodeDeletionResult::NodeUpdated;
                                }
                                2 => {
                                    children[current_node_child as usize] = None;
                                    let (orphan_nibble, orphan_ptr) = children
                                        .iter()
                                        .enumerate()
                                        .find(|(_, c)| c.is_some())
                                        .unwrap();
                                    let orphan_ptr = orphan_ptr.unwrap();

                                    if orphan_ptr.is_remote() {
                                        unreachable!("trie delete: orphan must be fetched before walking back");
                                    }

                                    deletion_result =
                                        NodeDeletionResult::BranchBelowRemovedWithOneChild {
                                            child_nibble: orphan_nibble as u8,
                                            child_ptr: orphan_ptr,
                                        };
                                }
                                _ => unreachable!(),
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                NodeDeletionResult::BranchBelowRemovedWithOneChild {
                    child_nibble: orphan_nibble,
                    child_ptr: orphan_ptr,
                } => {
                    // we need to merge orphaned node and its nibble into the new parent
                    let orphan_ptr_idx =
                        orphan_ptr.expect_local("deletion walking back: orphan is not in the trie");
                    let new_parent = self.nodes[current_node].clone();
                    let orphaned_node = self.nodes[orphan_ptr_idx].clone();
                    match (new_parent, orphaned_node) {
                        (
                            DiffTrieNode::Extension {
                                key: parent_key, ..
                            },
                            DiffTrieNode::Leaf {
                                key: orphan_key,
                                value,
                            },
                        ) => {
                            // replace extension node by merging its path into leaf with child_nibble
                            let new_leaf_key =
                                self.merge_keys(parent_key, orphan_nibble, orphan_key);
                            self.nodes[current_node] = DiffTrieNode::Leaf {
                                key: new_leaf_key,
                                value,
                            }
                        }
                        (
                            DiffTrieNode::Extension {
                                key: parent_key, ..
                            },
                            DiffTrieNode::Extension {
                                key: orphan_key,
                                next_node: orphan_next,
                            },
                        ) => {
                            // parent extension absorbs orphan extenion into itself
                            let new_ext_key =
                                self.merge_keys(parent_key, orphan_nibble, orphan_key);
                            self.nodes[current_node] = DiffTrieNode::Extension {
                                key: new_ext_key,
                                next_node: orphan_next,
                            }
                        }
                        (
                            DiffTrieNode::Extension {
                                key: parent_key, ..
                            },
                            DiffTrieNode::Branch { .. },
                        ) => {
                            // parent extension eats the orphan nibble and start to point to orphan branch
                            // orphan branch is not changed
                            let new_ext_key = self.merge_keys(parent_key, orphan_nibble, 0..0);
                            self.nodes[current_node] = DiffTrieNode::Extension {
                                key: new_ext_key,
                                next_node: orphan_ptr,
                            }
                        }
                        (
                            DiffTrieNode::Branch {
                                children: parent_children,
                            },
                            DiffTrieNode::Leaf {
                                key: orphan_key,
                                value,
                            },
                        ) => {
                            // parent branch starts to point to orphan leaf
                            // orphan leaf eats nibble
                            let new_leaf_key = self.merge_keys(0..0, orphan_nibble, orphan_key);
                            self.nodes[orphan_ptr_idx] = DiffTrieNode::Leaf {
                                key: new_leaf_key,
                                value,
                            };
                            self.hashed_nodes[orphan_ptr_idx] = false;
                            self.branch_node_children[parent_children]
                                [current_node_child as usize] = Some(orphan_ptr);
                        }
                        (
                            DiffTrieNode::Branch {
                                children: parent_children,
                            },
                            DiffTrieNode::Extension {
                                key: orphan_key,
                                next_node,
                            },
                        ) => {
                            // parent branch points to the orphan extension
                            // orphan extension eats nibble
                            let new_ext_key = self.merge_keys(0..0, orphan_nibble, orphan_key);
                            self.nodes[orphan_ptr_idx] = DiffTrieNode::Extension {
                                key: new_ext_key,
                                next_node,
                            };
                            self.hashed_nodes[orphan_ptr_idx] = false;
                            self.branch_node_children[parent_children]
                                [current_node_child as usize] = Some(orphan_ptr);
                        }
                        (
                            DiffTrieNode::Branch {
                                children: parent_children,
                            },
                            DiffTrieNode::Branch { .. },
                        ) => {
                            // parent branch points to new extension
                            // new extension node created with nibble inside pointing to orphan branch
                            // orphan branch is not changed
                            let new_ext_key = self.insert_key(&[orphan_nibble]);
                            let next_ext_ptr = self.push_node(DiffTrieNode::Extension {
                                key: new_ext_key,
                                next_node: orphan_ptr,
                            });
                            self.branch_node_children[parent_children]
                                [current_node_child as usize] = Some(next_ext_ptr);
                        }
                        _ => unreachable!(),
                    }
                    deletion_result = NodeDeletionResult::NodeUpdated;
                    break;
                }
                NodeDeletionResult::NodeUpdated => break,
            }
        }

        // here we handle the case when deletion reaches the head
        match deletion_result {
            // updates terminated before reaching the top
            NodeDeletionResult::NodeUpdated => {}
            NodeDeletionResult::NodeDeleted => {
                // trie is empty, insert the null node on top
                self.nodes[0] = DiffTrieNode::Null;
                self.hashed_nodes[0] = false;
            }
            // orphan becomes head
            NodeDeletionResult::BranchBelowRemovedWithOneChild {
                child_nibble: orphan_nibble,
                child_ptr: orphan_ptr,
            } => {
                let orphan_ptr_idx =
                    orphan_ptr.expect_local("deletion reached trie top: orphan is not in the trie");
                self.hashed_nodes[0] = false;
                match &self.nodes[orphan_ptr_idx] {
                    DiffTrieNode::Leaf {
                        key: orphan_key,
                        value,
                    } => {
                        let value = value.clone();
                        // orphan leaf eats nibble
                        let new_leaf_key = self.merge_keys(0..0, orphan_nibble, orphan_key.clone());
                        self.nodes[0] = DiffTrieNode::Leaf {
                            key: new_leaf_key,
                            value,
                        };
                    }
                    DiffTrieNode::Extension {
                        key: orphan_key,
                        next_node,
                    } => {
                        // orphan extension eats nibble
                        let next_node = *next_node;
                        let new_ext_key = self.merge_keys(0..0, orphan_nibble, orphan_key.clone());
                        self.nodes[0] = DiffTrieNode::Extension {
                            key: new_ext_key,
                            next_node,
                        }
                    }
                    DiffTrieNode::Branch { .. } => {
                        // new extension is created and it eats nibble
                        let new_ext_key = self.insert_key(&[orphan_nibble]);
                        self.nodes[0] = DiffTrieNode::Extension {
                            key: new_ext_key,
                            next_node: orphan_ptr,
                        }
                    }
                    DiffTrieNode::Null => unreachable!(),
                }
            }
        }

        Ok(old_value)
    }

    fn rlp_encode_node(&self, node_idx: usize, rlp: &mut Vec<u8>, proof_store: &ProofStore) {
        rlp.clear();
        let node = self
            .nodes
            .get(node_idx)
            .expect("rlp_encode_node: node not found")
            .clone();
        match node {
            DiffTrieNode::Branch { children } => {
                let remote_nodes = proof_store.rlp_ptrs();
                let mut children_rlp_ptrs: [Option<&[u8]>; 16] = Default::default();
                for (nibble, child) in self.branch_node_children[children].iter().enumerate() {
                    match child {
                        Some(NodePtr::Local(idx)) => {
                            debug_assert!(self.hashed_nodes[*idx]);
                            children_rlp_ptrs[nibble] = Some(self.rlp_ptrs_local[*idx].as_slice());
                        }
                        Some(NodePtr::Remote(idx)) => {
                            children_rlp_ptrs[nibble] = Some(remote_nodes[*idx].as_slice());
                        }
                        None => {}
                    }
                }
                encode_branch_node(&children_rlp_ptrs, rlp);
            }
            DiffTrieNode::Extension { key, next_node } => {
                let remote_nodes;

                let key = Nibbles::from_nibbles_unchecked(&self.keys[key]);
                let child_rlp_ptr = match next_node {
                    NodePtr::Local(idx) => {
                        debug_assert!(self.hashed_nodes[idx]);
                        self.rlp_ptrs_local[idx].as_slice()
                    }
                    NodePtr::Remote(idx) => {
                        remote_nodes = proof_store.rlp_ptrs();
                        remote_nodes[idx].as_slice()
                    }
                };
                encode_extension(&key, child_rlp_ptr, rlp);
            }
            DiffTrieNode::Leaf { key, value } => {
                let key = Nibbles::from_nibbles_unchecked(&self.keys[key]);
                encode_leaf(&key, &self.values[value], rlp);
            }
            DiffTrieNode::Null => {
                rlp.push(EMPTY_STRING_CODE);
            }
        }
    }

    // children must be hashed
    // NOT thread safe
    // this function is unsafe to avoid borrow checker when hashing node as it updates rlp_ptrs_local and hashed_node
    // 1. its not public
    // 2. pulic caller is root_hash and its &mut
    // 3. root_hash ensures that we don't have a race here
    fn calculate_rlp_pointer_node(
        &self,
        node_idx: usize,
        rlp: &mut Vec<u8>,
        proof_store: &ProofStore,
    ) {
        self.rlp_encode_node(node_idx, rlp, proof_store);
        let result =
            unsafe { &mut *(self.rlp_ptrs_local.as_ptr().add(node_idx) as *mut ArrayVec<u8, 33>) };
        result.clear();
        if rlp.len() < 32 {
            result.try_extend_from_slice(rlp).unwrap();
        } else {
            let hash = keccak256(rlp);
            result.push(EMPTY_STRING_CODE + 32);
            result.try_extend_from_slice(hash.as_slice()).unwrap();
        }

        let hashed_node = unsafe { &mut *(self.hashed_nodes.as_ptr().add(node_idx) as *mut bool) };
        *hashed_node = true;
    }

    /// Calculates hash of the trie, when parallel is set rayon is used.
    pub fn root_hash(
        &self,
        parallel: bool,
        proof_store: &ProofStore,
    ) -> Result<B256, NodeNotFound> {
        let mut rlp = Vec::new();
        if self.nodes.is_empty() {
            return Err(NodeNotFound(Nibbles::new()));
        }
        self.root_hash_node(0, &mut rlp, parallel, proof_store);
        self.rlp_encode_node(0, &mut rlp, proof_store);
        Ok(keccak256(&rlp))
    }

    fn root_hash_node(
        &self,
        node_idx: usize,
        rlp: &mut Vec<u8>,
        parallel: bool,
        proof_store: &ProofStore,
    ) {
        if self.hashed_nodes[node_idx] {
            return;
        }
        let node = self
            .nodes
            .get(node_idx)
            .expect("root_hash_node: node not found");
        match node {
            DiffTrieNode::Branch { children } => {
                let compute_children_this_thread = if !parallel {
                    true
                } else {
                    let local_children = self.branch_node_children[*children]
                        .iter()
                        .filter(|c| matches!(c, Some(NodePtr::Local(_))))
                        .count();
                    local_children <= 1
                };

                if compute_children_this_thread {
                    for child in self.branch_node_children[*children].into_iter().flatten() {
                        if let NodePtr::Local(child) = child {
                            self.root_hash_node(child, rlp, parallel, proof_store);
                        }
                    }
                } else {
                    rayon::scope(|scope| {
                        for child in self.branch_node_children[*children].into_iter().flatten() {
                            if let NodePtr::Local(child) = child {
                                scope.spawn(move |_| {
                                    let mut rlp = Vec::new();
                                    self.root_hash_node(child, &mut rlp, parallel, proof_store);
                                })
                            }
                        }
                    });
                }
                self.calculate_rlp_pointer_node(node_idx, rlp, proof_store);
            }
            DiffTrieNode::Extension { next_node, .. } => {
                if let NodePtr::Local(child) = next_node {
                    self.root_hash_node(*child, rlp, parallel, proof_store);
                }
                self.calculate_rlp_pointer_node(node_idx, rlp, proof_store);
            }
            DiffTrieNode::Null | DiffTrieNode::Leaf { .. } => {
                self.calculate_rlp_pointer_node(node_idx, rlp, proof_store);
            }
        }
    }

    pub fn get_proof(
        &self,
        key: &[u8],
        proof_store: &ProofStore,
    ) -> Result<ProofWithValue, ProofError> {
        let n = Nibbles::unpack(key);
        self.get_proof_nibbles_key(&n, proof_store)
    }

    /// Generate proof for the target key.
    pub fn get_proof_nibbles_key(
        &self,
        target_key: &Nibbles,
        proof_store: &ProofStore,
    ) -> Result<ProofWithValue, ProofError> {
        let mut buf = Vec::new();
        let mut result = ProofWithValue {
            proof: Vec::new(),
            value: None,
        };

        let mut current_node = 0;
        let mut path_walked = 0;

        loop {
            let node = self
                .nodes
                .get(current_node)
                .ok_or_else(|| NodeNotFound(target_key.clone()))?;

            if !self.hashed_nodes[current_node] {
                return Err(ProofError::TrieIsDirty);
            }

            self.rlp_encode_node(current_node, &mut buf, proof_store);
            let current_node_path =
                Nibbles::from_nibbles_unchecked(&target_key.as_slice()[..path_walked]);
            result.proof.push((current_node_path, buf.clone()));

            match node {
                DiffTrieNode::Branch { children } => {
                    if target_key.len() == path_walked {
                        break;
                    }

                    let children = *children;

                    let n = target_key[path_walked];
                    path_walked += 1;

                    if let Some(child_ptr) = self.branch_node_children[children][n as usize] {
                        current_node = child_ptr
                            .as_local()
                            .ok_or_else(|| NodeNotFound(target_key.clone()))?;
                        continue;
                    }

                    break;
                }
                DiffTrieNode::Extension { key, next_node } => {
                    let key = key.clone();
                    let next_node = *next_node;

                    if target_key[path_walked..].starts_with(&self.keys[key.clone()]) {
                        path_walked += key.len();
                        current_node = next_node
                            .as_local()
                            .ok_or_else(|| NodeNotFound(target_key.clone()))?;
                        continue;
                    }

                    break;
                }
                DiffTrieNode::Leaf { key, value } => {
                    if self.keys[key.clone()] == target_key[path_walked..] {
                        result.value = Some(self.values[value.clone()].to_vec());
                    }
                    break;
                }
                DiffTrieNode::Null => {
                    break;
                }
            }
        }

        Ok(result)
    }

    pub fn debug_print_node(&self, node_idx: usize) {
        let node = self
            .nodes
            .get(node_idx)
            .expect("print_node: node not found")
            .clone();
        let h = alloy_primitives::hex::encode;
        match node {
            DiffTrieNode::Branch { children } => {
                println!("{node_idx} Branch");
                println!("{}", h(self.rlp_ptrs_local[node_idx].as_slice()));
                for (idx, child) in self.branch_node_children[children].into_iter().enumerate() {
                    if child.is_some() {
                        println!("  {idx} -> {child:?}");
                    }
                }
                for child in self.branch_node_children[children].into_iter().flatten() {
                    if let NodePtr::Local(idx) = child {
                        self.debug_print_node(idx);
                    }
                }
            }
            DiffTrieNode::Extension { next_node, key } => {
                println!(
                    "{} Extension {:?} -> {:?}",
                    node_idx,
                    h(&self.keys[key]),
                    next_node
                );
                println!("{}", h(self.rlp_ptrs_local[node_idx].as_slice()));
                if let NodePtr::Local(idx) = next_node {
                    self.debug_print_node(idx);
                }
            }
            DiffTrieNode::Leaf { key, value } => {
                println!(
                    "{} Leaf {:?} : {:?}",
                    node_idx,
                    h(&self.keys[key]),
                    h(&self.values[value])
                );
                println!("{}", h(self.rlp_ptrs_local[node_idx].as_slice()));
            }
            DiffTrieNode::Null => {
                println!("{node_idx} Null");
                println!("{}", h(self.rlp_ptrs_local[node_idx].as_slice()));
            }
        }
    }

    pub fn try_add_proof_from_proof_store(
        &mut self,
        key: &Nibbles,
        proof_store: &ProofStore,
    ) -> Result<bool, NodeNotFound> {
        let proof = if let Some(proof) = proof_store.proofs.get(key) {
            proof
        } else {
            return Ok(false);
        };

        for (path, node) in proof.value() {
            self.add_node_from_proof(path, node, proof_store)?;
        }

        Ok(true)
    }

    // node can be added only if all of its parents are actually in the trie
    fn add_node_from_proof(
        &mut self,
        path: &Nibbles,
        node: &ProofNode,
        proof_store: &ProofStore,
    ) -> Result<(), NodeNotFound> {
        if path.is_empty() && !self.nodes.is_empty() {
            return Ok(());
        }

        if self.nodes.is_empty() {
            if !path.is_empty() {
                return Err(NodeNotFound(Nibbles::new()));
            }
            match node {
                ProofNode::Branch { children } => {
                    let branch_node_children = self.create_branch_children();
                    let branch_children = &mut self.branch_node_children[branch_node_children];
                    for b in 0..16 {
                        if let Some(child_rlp) = &children[b] {
                            let child_ptr = NodePtr::Remote(*child_rlp);
                            branch_children[b] = Some(child_ptr);
                        }
                    }
                    self.push_node(DiffTrieNode::Branch {
                        children: branch_node_children,
                    });
                }
                ProofNode::Extension { key, child } => {
                    let key = &proof_store.keys_guard()[*key];
                    let key = self.insert_key(key);
                    let next_node = NodePtr::Remote(*child);
                    self.push_node(DiffTrieNode::Extension { key, next_node });
                }
                ProofNode::Leaf { key, value } => {
                    let key = &proof_store.keys_guard()[*key];
                    let key = self.insert_key(key);
                    let value = &proof_store.values_guard()[*value];
                    let value = self.copy_value(value);
                    self.push_node(DiffTrieNode::Leaf { key, value });
                }
                ProofNode::Empty => {
                    self.push_node(DiffTrieNode::Null);
                }
            }
            return Ok(());
        }

        let mut current_node = 0;
        let mut path_walked = 0;

        let mut parent_ptr = None;
        let mut parent_nibble = 0;

        loop {
            let node = self
                .nodes
                .get(current_node)
                .ok_or_else(|| NodeNotFound::new(&path[..path_walked]))?;
            self.hashed_nodes[current_node] = false;
            match node {
                DiffTrieNode::Branch { children } => {
                    let children = *children;

                    let n = path[path_walked] as usize;
                    path_walked += 1;
                    if path[path_walked..].is_empty() {
                        parent_ptr = self.branch_node_children[children][n];
                        parent_nibble = n;
                        break;
                    }
                    if let Some(child_ptr) = self.branch_node_children[children][n] {
                        current_node = child_ptr
                            .as_local()
                            .ok_or_else(|| NodeNotFound::new(&path[..path_walked]))?;
                        continue;
                    } else {
                        return Err(NodeNotFound::new(&path[..path_walked]));
                    }
                }
                DiffTrieNode::Extension { key, next_node } => {
                    let key = key.clone();
                    let next_node = *next_node;

                    if path[path_walked..].starts_with(&self.keys[key.clone()]) {
                        path_walked += key.len();

                        if path[path_walked..].is_empty() {
                            parent_ptr = Some(next_node);
                            parent_nibble = 0;
                            break;
                        }
                        current_node = next_node.as_local().ok_or_else(|| {
                            NodeNotFound(Nibbles::from_nibbles_unchecked(&path[..path_walked]))
                        })?;
                        continue;
                    }
                }
                _ => {
                    // no proofs can be added here,
                    return Ok(());
                }
            }
            break;
        }

        match parent_ptr {
            Some(NodePtr::Remote(_)) => {}
            _ => {
                // node is not needed
                return Ok(());
            }
        };

        let new_node = match node {
            ProofNode::Leaf { key, value } => {
                let key = &proof_store.keys_guard()[*key];
                let key = self.insert_key(key);
                let value = &proof_store.values_guard()[*value];
                let value = self.copy_value(value);
                self.push_node(DiffTrieNode::Leaf { key, value })
            }
            ProofNode::Extension { key, child } => {
                let key = &proof_store.keys_guard()[*key];
                let key = self.insert_key(key);
                let next_node = NodePtr::Remote(*child);
                self.push_node(DiffTrieNode::Extension { key, next_node })
            }
            ProofNode::Branch { children } => {
                let branch_node_children = self.create_branch_children();
                for b in 0..16 {
                    if let Some(child_rlp) = &children[b] {
                        let child_ptr = NodePtr::Remote(*child_rlp);
                        self.branch_node_children[branch_node_children][b] = Some(child_ptr);
                    }
                }
                self.push_node(DiffTrieNode::Branch {
                    children: branch_node_children,
                })
            }
            ProofNode::Empty => panic!("inserting empty to node to non empty trie"),
        };

        // give pointer to a parent
        match &mut self.nodes[current_node] {
            DiffTrieNode::Branch { children } => {
                self.branch_node_children[*children][parent_nibble] = Some(new_node);
            }
            DiffTrieNode::Extension { next_node, .. } => {
                *next_node = new_node;
            }
            _ => unreachable!(),
        }

        Ok(())
    }
}
