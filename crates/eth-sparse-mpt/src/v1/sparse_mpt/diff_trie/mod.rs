use crate::utils::{extract_prefix_and_suffix, rlp_pointer, strip_first_nibble_mut, HashMap};
use alloy_primitives::{keccak256, Bytes, B256};
use nybbles::Nibbles;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, Seq};

mod nodes;

#[cfg(test)]
mod tests;

pub use nodes::*;

#[serde_as]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiffTrie {
    #[serde_as(as = "Seq<(_, _)>")]
    pub nodes: HashMap<u64, DiffTrieNode>,
    pub head: u64,
    pub ptrs: u64,
}

impl DiffTrie {
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn new_empty() -> Self {
        Self {
            nodes: [(0, DiffTrieNode::new_null())].into_iter().collect(),
            head: 0,
            ptrs: 0,
        }
    }

    pub fn clear_empty(&mut self) {
        self.nodes.clear();
        self.head = 0;
        self.ptrs = 0;
        self.nodes.insert(0, DiffTrieNode::new_null());
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Node not found")]
pub struct ErrSparseNodeNotFound {
    ptr: u64,
    path: Nibbles,
}

#[derive(Debug, thiserror::Error)]
pub enum DeletionError {
    #[error("Deletion error: {0:?}")]
    NodeNotFound(#[from] ErrSparseNodeNotFound),
    #[error("Key node not found in the trie")]
    KeyNotFound,
}

#[derive(Debug)]
pub struct NodeCursor {
    pub current_node: u64,
    pub current_path: Nibbles,
    pub path_left: Nibbles,
}

impl NodeCursor {
    pub fn new(key: Nibbles, head: u64) -> Self {
        let current_path = Nibbles::with_capacity(key.len());
        Self {
            current_node: head,
            current_path,
            path_left: key,
        }
    }

    pub fn step_into_extension(&mut self, ext: &DiffExtensionNode) {
        let len = ext.key().len();
        self.current_path
            .extend_from_slice_unchecked(&self.path_left[..len]);
        self.path_left.as_mut_vec_unchecked().drain(..len);
        self.current_node = ext.child.ptr();
    }

    pub fn next_nibble(&self) -> u8 {
        self.path_left.first().unwrap()
    }

    pub fn step_into_branch(&mut self, branch: &DiffBranchNode) -> u8 {
        let nibble = strip_first_nibble_mut(&mut self.path_left);
        self.current_path.push_unchecked(nibble);
        self.current_node = branch
            .get_diff_child(nibble)
            .map(|c| c.ptr())
            .unwrap_or(u64::MAX);
        nibble
    }
}

fn try_get_node_mut<'a>(
    nodes: &'a mut HashMap<u64, DiffTrieNode>,
    ptr: u64,
    path: &Nibbles,
) -> Result<&'a mut DiffTrieNode, ErrSparseNodeNotFound> {
    nodes.get_mut(&ptr).ok_or_else(|| ErrSparseNodeNotFound {
        path: path.clone(),
        ptr,
    })
}

pub fn get_new_ptr(ptrs: &mut u64) -> u64 {
    *ptrs += 1;
    *ptrs
}

impl DiffTrie {
    pub fn insert(&mut self, key: Bytes, value: Bytes) -> Result<(), ErrSparseNodeNotFound> {
        let key = Nibbles::unpack(key);
        let mut c = NodeCursor::new(key, self.head);

        let mut new_nodes: Vec<(u64, DiffTrieNode)> = Vec::with_capacity(0);

        loop {
            let node = try_get_node_mut(&mut self.nodes, c.current_node, &c.current_path)?;
            match &mut node.kind {
                DiffTrieNodeKind::Null => {
                    let new_node = DiffTrieNode::new_leaf(c.path_left, value);
                    *node = new_node;
                    break;
                }
                DiffTrieNodeKind::Leaf(leaf) => {
                    if leaf.key() == &c.path_left {
                        // update leaf inplace
                        leaf.changed_value = Some(value);
                        node.rlp_pointer = None;
                        break;
                    }

                    let (pref, mut suff1, mut suff2) =
                        extract_prefix_and_suffix(&c.path_left, leaf.key());
                    assert!(suff1.len() == suff2.len() && !suff1.is_empty(), "inserting into the leaf using different key lengths (key lengths must be constant)");

                    let n1 = strip_first_nibble_mut(&mut suff1);
                    let n2 = strip_first_nibble_mut(&mut suff2);
                    let (key1, key2) = (suff1, suff2);

                    let leaf1_ptr = get_new_ptr(&mut self.ptrs);
                    let leaf2_ptr = get_new_ptr(&mut self.ptrs);

                    new_nodes.reserve(3);
                    new_nodes.push((leaf1_ptr, DiffTrieNode::new_leaf(key1, value)));
                    new_nodes.push((
                        leaf2_ptr,
                        DiffTrieNode::new_leaf(key2, leaf.value().clone()),
                    ));

                    let branch = DiffTrieNode::new_branch(
                        n1,
                        DiffChildPtr::new(leaf1_ptr),
                        n2,
                        DiffChildPtr::new(leaf2_ptr),
                    );

                    let replace_current_node = if pref.is_empty() {
                        branch
                    } else {
                        let branch_ptr = get_new_ptr(&mut self.ptrs);
                        new_nodes.push((branch_ptr, branch));
                        DiffTrieNode::new_ext(pref, DiffChildPtr::new(branch_ptr))
                    };

                    *node = replace_current_node;
                    break;
                }
                DiffTrieNodeKind::Extension(extension) => {
                    if c.path_left.starts_with(extension.key()) {
                        // pass insertion deeper
                        extension.child.mark_dirty();
                        node.rlp_pointer = None;
                        c.step_into_extension(extension);
                        continue;
                    }

                    let (pref, mut suff1, mut suff2) =
                        extract_prefix_and_suffix(&c.path_left, extension.key());
                    assert!(
                        !suff2.is_empty(),
                        "inserting into the extension node while we should go deeper"
                    );
                    assert!(
                        !suff1.is_empty(),
                        "trying to insert value into the branch node (key lengths must be constant)"
                    );

                    let n1 = strip_first_nibble_mut(&mut suff1);
                    let n2 = strip_first_nibble_mut(&mut suff2);
                    let (key1, key2) = (suff1, suff2);

                    let leaf_ptr = get_new_ptr(&mut self.ptrs);
                    new_nodes.reserve(3);
                    new_nodes.push((leaf_ptr, DiffTrieNode::new_leaf(key1, value)));

                    // branch will point to the current extension child directly or to new extension node
                    // that will point to child

                    let branch_other_child = if !key2.is_empty() {
                        let new_ext_ptr = get_new_ptr(&mut self.ptrs);
                        let new_ext = DiffTrieNode::new_ext(key2, extension.child.clone());
                        new_nodes.push((new_ext_ptr, new_ext));
                        DiffChildPtr::new(new_ext_ptr)
                    } else {
                        extension.child.clone()
                    };

                    let branch_node = DiffTrieNode::new_branch(
                        n1,
                        DiffChildPtr::new(leaf_ptr),
                        n2,
                        branch_other_child,
                    );

                    let replace_current_node = if pref.is_empty() {
                        branch_node
                    } else {
                        let branch_ptr = get_new_ptr(&mut self.ptrs);
                        new_nodes.push((branch_ptr, branch_node));
                        DiffTrieNode::new_ext(pref, DiffChildPtr::new(branch_ptr))
                    };

                    *node = replace_current_node;
                    break;
                }
                DiffTrieNodeKind::Branch(branch) => {
                    assert!(
                        !c.path_left.is_empty(),
                        "inserting value into a branch node (key lengths must be constant)"
                    );
                    node.rlp_pointer = None;
                    let n = c.step_into_branch(branch);

                    if branch.has_child(n) {
                        let child =
                            branch
                                .get_diff_child_mut(n)
                                .ok_or_else(|| ErrSparseNodeNotFound {
                                    path: c.current_path.clone(),
                                    ptr: u64::MAX,
                                })?;
                        child.mark_dirty();
                        continue;
                    } else {
                        let leaf_ptr = get_new_ptr(&mut self.ptrs);
                        new_nodes.push((leaf_ptr, DiffTrieNode::new_leaf(c.path_left, value)));
                        branch.insert_diff_child(n, DiffChildPtr::new(leaf_ptr));
                        break;
                    }
                }
            }
        }

        for (path, node) in new_nodes {
            self.nodes.insert(path, node);
        }

        Ok(())
    }

    pub fn delete(&mut self, key: Bytes) -> Result<(), DeletionError> {
        let key = Nibbles::unpack(key);
        let mut c = NodeCursor::new(key, self.head);

        let mut walk_path: Vec<(u64, u8)> = Vec::new();

        loop {
            let node = try_get_node_mut(&mut self.nodes, c.current_node, &c.current_path)
                .map_err(DeletionError::NodeNotFound)?;

            match &mut node.kind {
                DiffTrieNodeKind::Null => {
                    return Err(DeletionError::KeyNotFound);
                }
                DiffTrieNodeKind::Leaf(leaf) => {
                    if leaf.key() == &c.path_left {
                        walk_path.push((c.current_node, 0));
                        break;
                    } else {
                        return Err(DeletionError::KeyNotFound);
                    }
                }
                DiffTrieNodeKind::Extension(extension) => {
                    if !c.path_left.starts_with(extension.key()) {
                        return Err(DeletionError::KeyNotFound);
                    }
                    walk_path.push((c.current_node, 0));
                    c.step_into_extension(extension);

                    // pass deletion deeper
                    extension.child.mark_dirty();
                    node.rlp_pointer = None;
                    continue;
                }
                DiffTrieNodeKind::Branch(branch) => {
                    if c.path_left.is_empty() {
                        // trying to delete key from branch
                        return Err(DeletionError::KeyNotFound);
                    }

                    let branch_node_path = c.current_node;
                    let n = c.step_into_branch(branch);

                    walk_path.push((branch_node_path, n));

                    if !branch.has_child(n) {
                        return Err(DeletionError::KeyNotFound);
                    }

                    let child = branch.get_diff_child_mut(n).ok_or_else(|| {
                        DeletionError::NodeNotFound(ErrSparseNodeNotFound {
                            path: c.current_path.clone(),
                            ptr: u64::MAX,
                        })
                    })?;
                    child.mark_dirty();
                    node.rlp_pointer = None;

                    // check if we are removing from the branch with one child and we don't have a child
                    // its important to do it here so we don't modify the trie
                    // @note, this may be too strict as we only need to check that for branches on the bottom of the trie
                    let child_count = branch.child_count();
                    if child_count == 2 {
                        // @test add test for this code path
                        let other_child_nibble = branch
                            .other_child_nibble(n)
                            .expect("other child must exist");
                        if branch.get_diff_child(other_child_nibble).is_none() {
                            let mut other_child_path = c.current_path.clone();
                            if let Some(l) = other_child_path.as_mut_vec_unchecked().last_mut() {
                                *l = other_child_nibble;
                            }
                            return Err(DeletionError::NodeNotFound(ErrSparseNodeNotFound {
                                path: other_child_path,
                                ptr: u64::MAX,
                            }));
                        }
                    }
                    continue;
                }
            }
        }

        // now we walk our path back

        #[derive(Debug)]
        enum NodeDeletionResult {
            NodeDeleted,
            NodeUpdated,
            BranchBelowRemovedWithOneChild { child_nibble: u8, child_ptr: u64 },
        }

        let mut deletion_result = NodeDeletionResult::NodeDeleted;

        for (current_node, current_node_child) in walk_path.into_iter().rev() {
            match &mut deletion_result {
                NodeDeletionResult::NodeDeleted => {
                    let node = try_get_node_mut(&mut self.nodes, current_node, &Nibbles::new())
                        .expect("nodes must exist when walking back");
                    let should_remove = match &mut node.kind {
                        DiffTrieNodeKind::Null => unreachable!(),
                        DiffTrieNodeKind::Leaf(_) => {
                            deletion_result = NodeDeletionResult::NodeDeleted;
                            true
                        }
                        DiffTrieNodeKind::Extension(_) => {
                            // Only branch nodes can be children of the extension nodes
                            // to remove branch node in sec trie we must remove all of its children
                            // but when we remove the second last children and left with one branch node
                            // will trigger BranchBelowRemovedWithOneChild code path so this code path will never
                            // be reachable
                            unreachable!("Child of the extension node can't be deleted in sec trie")
                        }
                        DiffTrieNodeKind::Branch(branch) => {
                            let child_count = branch.child_count();
                            match child_count {
                                0..=1 => {
                                    unreachable!("removing last child or removing from branch without children")
                                }
                                2 => {
                                    // removing one but last child, remove branch node and bubble the deletion up
                                    let (other_child_ptr, other_child_nibble) = branch
                                        .other_child_ptr_and_nibble(current_node_child)
                                        .expect("other child must exist");
                                    deletion_result =
                                        NodeDeletionResult::BranchBelowRemovedWithOneChild {
                                            child_nibble: other_child_nibble,
                                            child_ptr: other_child_ptr,
                                        };
                                    true
                                }
                                3.. => {
                                    branch.delete_child(current_node_child);
                                    deletion_result = NodeDeletionResult::NodeUpdated;
                                    break;
                                }
                            }
                        }
                    };
                    if should_remove {
                        self.nodes
                            .remove(&current_node)
                            .expect("when deleting node it should be in the trie");
                    }
                }
                NodeDeletionResult::NodeUpdated => break,
                NodeDeletionResult::BranchBelowRemovedWithOneChild {
                    child_nibble,
                    child_ptr,
                } => {
                    let child_below = self
                        .nodes
                        .remove(child_ptr)
                        .expect("orphaned child existence is checked when walking down");
                    let node_above =
                        try_get_node_mut(&mut self.nodes, current_node, &Nibbles::new())
                            .expect("nodes must exist when walking back");
                    let mut reinsert_nodes: Vec<(u64, DiffTrieNode)> = Vec::with_capacity(2);
                    match (&mut node_above.kind, child_below.kind) {
                        (
                            DiffTrieNodeKind::Extension(ext_above),
                            DiffTrieNodeKind::Leaf(leaf_below),
                        ) => {
                            // we just replace extension node by merging its path into leaf with child_nibble
                            let mut new_leaf_key = ext_above.key().clone();
                            new_leaf_key.push(*child_nibble);
                            new_leaf_key.extend_from_slice_unchecked(leaf_below.key());

                            let mut new_leaf = leaf_below;
                            new_leaf.changed_key = Some(new_leaf_key);
                            node_above.kind = DiffTrieNodeKind::Leaf(new_leaf);
                        }
                        (
                            DiffTrieNodeKind::Extension(ext_above),
                            DiffTrieNodeKind::Extension(ext_below),
                        ) => {
                            // we merge two extension nodes into current node with child_nibble
                            let ext_key = ext_above.key_mut();
                            ext_key.push(*child_nibble);
                            ext_key.extend_from_slice_unchecked(ext_below.key());

                            ext_above.child = ext_below.child.clone();
                        }
                        (
                            DiffTrieNodeKind::Extension(ext_above),
                            DiffTrieNodeKind::Branch(branch),
                        ) => {
                            // we consume remove child nibble into extension node and reinsert branch into the trie
                            // but with a different path
                            ext_above.key_mut().push(*child_nibble);

                            let new_branch_ptr = get_new_ptr(&mut self.ptrs);
                            ext_above.child = DiffChildPtr::new(new_branch_ptr);

                            let new_child = DiffTrieNode {
                                kind: DiffTrieNodeKind::Branch(branch),
                                rlp_pointer: child_below.rlp_pointer,
                            };
                            reinsert_nodes.push((new_branch_ptr, new_child));
                        }
                        (
                            DiffTrieNodeKind::Branch(branch_above),
                            DiffTrieNodeKind::Leaf(mut leaf_below),
                        ) => {
                            // merge missing nibble into the leaf
                            leaf_below
                                .key_mut()
                                .as_mut_vec_unchecked()
                                .insert(0, *child_nibble);

                            let new_leaf_ptr = get_new_ptr(&mut self.ptrs);
                            let new_child = DiffTrieNode {
                                kind: DiffTrieNodeKind::Leaf(leaf_below),
                                rlp_pointer: None,
                            };
                            reinsert_nodes.push((new_leaf_ptr, new_child));

                            branch_above.insert_diff_child(
                                current_node_child,
                                DiffChildPtr::new(new_leaf_ptr),
                            );
                        }
                        (
                            DiffTrieNodeKind::Branch(branch_above),
                            DiffTrieNodeKind::Extension(mut ext_below),
                        ) => {
                            // merge missing nibble into the extension
                            ext_below
                                .key_mut()
                                .as_mut_vec_unchecked()
                                .insert(0, *child_nibble);
                            let new_child_ptr = get_new_ptr(&mut self.ptrs);
                            let new_child = DiffTrieNode {
                                kind: DiffTrieNodeKind::Extension(ext_below),
                                rlp_pointer: None,
                            };
                            reinsert_nodes.push((new_child_ptr, new_child));

                            branch_above.insert_diff_child(
                                current_node_child,
                                DiffChildPtr::new(new_child_ptr),
                            );
                        }
                        (
                            DiffTrieNodeKind::Branch(branch_above),
                            DiffTrieNodeKind::Branch(branch_below),
                        ) => {
                            let reinsert_branch_ptr = get_new_ptr(&mut self.ptrs);
                            // we leave branch in the trie but create extension node instead of the remove one child node
                            let new_ext_ptr = get_new_ptr(&mut self.ptrs);
                            let new_ext_node = DiffTrieNode::new_ext(
                                Nibbles::from_nibbles_unchecked([*child_nibble]),
                                DiffChildPtr::new(reinsert_branch_ptr),
                            );
                            branch_above.insert_diff_child(
                                current_node_child,
                                DiffChildPtr::new(new_ext_ptr),
                            );

                            let reinsert_branch_node = DiffTrieNode {
                                kind: DiffTrieNodeKind::Branch(branch_below),
                                rlp_pointer: child_below.rlp_pointer,
                            };

                            reinsert_nodes.push((new_ext_ptr, new_ext_node));
                            reinsert_nodes.push((reinsert_branch_ptr, reinsert_branch_node));
                        }
                        _ => unreachable!(),
                    }

                    for (ptr, node) in reinsert_nodes {
                        self.nodes.insert(ptr, node);
                    }

                    deletion_result = NodeDeletionResult::NodeUpdated;
                }
            }
        }

        // here we handle the case on top of the trie
        match deletion_result {
            NodeDeletionResult::NodeDeleted => {
                let ptr = get_new_ptr(&mut self.ptrs);
                // trie is empty, insert the null node on top
                self.nodes.insert(ptr, DiffTrieNode::new_null());
                self.head = ptr;
            }
            NodeDeletionResult::BranchBelowRemovedWithOneChild {
                child_nibble,
                child_ptr,
            } => {
                let mut reinsert_nodes = Vec::with_capacity(0);

                let mut child_below = self
                    .nodes
                    .remove(&child_ptr)
                    .expect("orphaned child existence verif");
                match &mut child_below.kind {
                    DiffTrieNodeKind::Leaf(leaf) => {
                        leaf.key_mut()
                            .as_mut_vec_unchecked()
                            .insert(0, child_nibble);
                        child_below.rlp_pointer = None;
                    }
                    DiffTrieNodeKind::Extension(ext) => {
                        ext.key_mut().as_mut_vec_unchecked().insert(0, child_nibble);
                        child_below.rlp_pointer = None;
                    }
                    DiffTrieNodeKind::Branch(_) => {
                        // create extension node with the nibble and the child
                        let reinsert_branch_ptr = get_new_ptr(&mut self.ptrs);
                        reinsert_nodes.push((reinsert_branch_ptr, child_below.clone()));
                        child_below.kind = DiffTrieNodeKind::Extension(DiffExtensionNode {
                            fixed: None,
                            changed_key: Some(Nibbles::from_nibbles_unchecked([child_nibble])),
                            child: DiffChildPtr::new(reinsert_branch_ptr),
                        });
                        child_below.rlp_pointer = None;
                    }
                    DiffTrieNodeKind::Null => unreachable!(),
                };
                let ptr = get_new_ptr(&mut self.ptrs);
                self.head = ptr;
                self.nodes.insert(ptr, child_below);
                for (ptr, node) in reinsert_nodes {
                    self.nodes.insert(ptr, node);
                }
            }
            NodeDeletionResult::NodeUpdated => {}
        }

        Ok(())
    }

    pub fn root_hash(&mut self) -> Result<B256, ErrSparseNodeNotFound> {
        struct WaitStack {
            node: u64,
            stack_before: usize,
            need_elements: usize,
        }

        let mut task_stack: Vec<u64> = vec![self.head];
        let mut wait_stack: Vec<WaitStack> = Vec::new();
        let mut result_stack: Vec<Bytes> = Vec::new();

        let empty_path = Nibbles::new();

        while let Some(current_node) = task_stack.pop() {
            let node = try_get_node_mut(&mut self.nodes, current_node, &empty_path)?;

            match &mut node.kind {
                DiffTrieNodeKind::Null | DiffTrieNodeKind::Leaf(_) => {
                    result_stack.push(node.rlp_pointer_slow());
                }
                DiffTrieNodeKind::Extension(extension) => {
                    if node.rlp_pointer.is_none() && extension.child.rlp_pointer.is_none() {
                        let child_node = extension.child.ptr();
                        wait_stack.push(WaitStack {
                            node: current_node,
                            stack_before: result_stack.len(),
                            need_elements: 1,
                        });
                        task_stack.push(child_node);
                    } else {
                        result_stack.push(node.rlp_pointer_slow());
                    }
                }
                DiffTrieNodeKind::Branch(branch_node) => {
                    if node.rlp_pointer.is_none() {
                        let mut need_elements = 0;
                        for (_, child) in &branch_node.changed_children {
                            if let Some(child) = child {
                                if child.rlp_pointer.is_none() {
                                    need_elements += 1;
                                    task_stack.push(child.ptr());
                                }
                            }
                        }

                        if need_elements == 0 {
                            result_stack.push(node.rlp_pointer_slow());
                        } else {
                            wait_stack.push(WaitStack {
                                node: current_node,
                                stack_before: result_stack.len(),
                                need_elements,
                            });
                        }
                    } else {
                        result_stack.push(node.rlp_pointer_slow());
                    }
                }
            }
            while let Some(w) = wait_stack.last() {
                if result_stack.len() < w.need_elements + w.stack_before {
                    break;
                }
                let wait = wait_stack.pop().unwrap();
                let node = try_get_node_mut(&mut self.nodes, wait.node, &empty_path)?;
                let idx = result_stack.len() - wait.need_elements;
                update_node_with_calculated_dirty_children(node, result_stack.drain(idx..).rev());
                result_stack.push(node.rlp_pointer_slow());
            }
        }

        assert!(task_stack.is_empty());
        assert!(wait_stack.is_empty());
        assert_eq!(result_stack.len(), 1);

        let head = try_get_node_mut(&mut self.nodes, self.head, &empty_path)?;
        Ok(keccak256(head.rlp_encode(&[])))
    }

    fn root_hash_parallel_nodes(&self, node_ptr: u64) -> Bytes {
        let node = self.nodes.get(&node_ptr).expect("node not found");
        let mut child_rlp = Vec::new();
        let rlp_encode = match &node.kind {
            DiffTrieNodeKind::Null | DiffTrieNodeKind::Leaf(_) => node.rlp_encode(&[]),
            DiffTrieNodeKind::Extension(extension) => {
                if node.rlp_pointer.is_none() && extension.child.rlp_pointer.is_none() {
                    let child_node = extension.child.ptr();
                    let child_bytes = rlp_pointer(self.root_hash_parallel_nodes(child_node));
                    child_rlp.push(child_bytes.clone());
                    node.rlp_encode(&[child_bytes])
                } else {
                    node.rlp_encode(&[])
                }
            }
            DiffTrieNodeKind::Branch(branch_node) => {
                if node.rlp_pointer.is_none() {
                    let mut need_elements = Vec::new();
                    for (_, child) in &branch_node.changed_children {
                        if let Some(child) = child {
                            if child.rlp_pointer.is_none() {
                                need_elements.push(child.ptr());
                            }
                        }
                    }

                    if need_elements.is_empty() {
                        node.rlp_encode(&[])
                    } else {
                        let results = if need_elements.len() <= 2 {
                            let mut res = Vec::with_capacity(need_elements.len());
                            for child in need_elements.into_iter() {
                                res.push(rlp_pointer(self.root_hash_parallel_nodes(child)));
                            }
                            res
                        } else {
                            let res = Mutex::new(Vec::with_capacity(need_elements.len()));
                            rayon::scope(|scope| {
                                for (idx, child) in need_elements.iter().enumerate() {
                                    let res = &res;
                                    scope.spawn(move |_| {
                                        let data =
                                            rlp_pointer(self.root_hash_parallel_nodes(*child));
                                        res.lock().push((idx, data));
                                    });
                                }
                            });
                            let mut results = res.lock();
                            results.sort_by_key(|(i, _)| *i);
                            let results: Vec<_> = results.iter().map(|(_, b)| b.clone()).collect();
                            results
                        };
                        child_rlp.extend_from_slice(&results);
                        node.rlp_encode(&results)
                    }
                } else {
                    node.rlp_encode(&[])
                }
            }
        };
        rlp_encode
    }

    /// Calculate root hash of the trie in parallel using rayon.
    /// NOTE: it will not update dirty status of the nodes in the trie
    /// for now this is not used in a trie but will be necessary if we want to cache paths from previous iterations
    pub fn root_hash_parallel(&mut self) -> Result<B256, ErrSparseNodeNotFound> {
        let encode = self.root_hash_parallel_nodes(self.head);
        Ok(keccak256(&encode))
    }

    pub fn print(&self) {
        println!("head {}", self.head);
        println!("ptrs {}", self.ptrs);
        let mut keys = self.nodes.keys().collect::<Vec<_>>();
        keys.sort();
        for key in keys {
            println!("node {} {:#?}", key, self.nodes.get(key).unwrap());
        }
    }
}

fn update_node_with_calculated_dirty_children(
    node: &mut DiffTrieNode,
    mut dirty_branches: impl Iterator<Item = Bytes>,
) {
    match &mut node.kind {
        DiffTrieNodeKind::Extension(ext) => {
            let child_ptr = dirty_branches.next().expect("must have ext child");
            ext.child.rlp_pointer = Some(child_ptr);
        }
        DiffTrieNodeKind::Branch(branch) => {
            for (_, child) in &mut branch.changed_children {
                if let Some(child) = child {
                    if child.rlp_pointer.is_none() {
                        child.rlp_pointer =
                            Some(dirty_branches.next().expect("must have branch child"));
                    }
                }
            }
        }
        _ => unreachable!(),
    }
}
