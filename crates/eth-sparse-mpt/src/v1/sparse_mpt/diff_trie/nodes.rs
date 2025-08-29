use crate::utils::{
    encode_branch_node, encode_extension, encode_leaf, encode_len_branch_node,
    encode_len_extension, encode_len_leaf, encode_null_node, rlp_pointer,
};
use alloy_primitives::Bytes;
use nybbles::Nibbles;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::sync::Arc;

use super::super::fixed_trie::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffTrieNode {
    pub kind: DiffTrieNodeKind,
    // None means that node is dirty and hash recalculation is needed
    pub rlp_pointer: Option<Bytes>,
}

impl DiffTrieNode {
    pub fn new_null() -> Self {
        Self {
            kind: DiffTrieNodeKind::Null,
            rlp_pointer: None,
        }
    }

    pub fn new_leaf(key: Nibbles, value: Bytes) -> Self {
        Self {
            kind: DiffTrieNodeKind::Leaf(DiffLeafNode {
                fixed: None,
                changed_key: Some(key),
                changed_value: Some(value),
            }),
            rlp_pointer: None,
        }
    }

    pub fn new_branch(n1: u8, ptr1: DiffChildPtr, n2: u8, ptr2: DiffChildPtr) -> Self {
        assert!(n1 != n2);
        let mut changed_children = SmallVec::new();
        let (n1, ptr1, n2, ptr2) = if n1 > n2 {
            (n2, ptr2, n1, ptr1)
        } else {
            (n1, ptr1, n2, ptr2)
        };
        changed_children.push((n1, Some(ptr1)));
        changed_children.push((n2, Some(ptr2)));
        Self {
            kind: DiffTrieNodeKind::Branch(DiffBranchNode {
                fixed: None,
                changed_children,
                aux_bits: 0,
            }),
            rlp_pointer: None,
        }
    }

    pub fn new_ext(key: Nibbles, child: DiffChildPtr) -> Self {
        Self {
            kind: DiffTrieNodeKind::Extension(DiffExtensionNode {
                fixed: None,
                changed_key: Some(key),
                child,
            }),
            rlp_pointer: None,
        }
    }

    pub fn rlp_pointer_slow(&mut self) -> Bytes {
        if let Some(rlp_pointer) = &self.rlp_pointer {
            return rlp_pointer.clone();
        }

        let encode = self.rlp_encode(&[]);

        let rlp_pointer = rlp_pointer(encode);
        self.rlp_pointer = Some(rlp_pointer.clone());
        rlp_pointer
    }

    pub fn rlp_encode(&self, dirty_children: &[Bytes]) -> Bytes {
        let out = match &self.kind {
            DiffTrieNodeKind::Leaf(leaf) => {
                let (key, value) = (leaf.key(), leaf.value());
                let len = encode_len_leaf(key, value);
                let mut out = Vec::with_capacity(len);
                encode_leaf(key, value, &mut out);
                out
            }
            DiffTrieNodeKind::Extension(ext) => {
                let (key, child_rlp) = (
                    ext.key(),
                    &ext.child
                        .rlp_pointer
                        .as_ref()
			.or_else(|| dirty_children.first())
                        .expect("ext node rlp: child rlp must be computed or provided as a dirty children arg"),
                );
                let len = encode_len_extension(key, child_rlp);
                let mut out = Vec::with_capacity(len);
                encode_extension(key, child_rlp, &mut out);
                out
            }
            DiffTrieNodeKind::Branch(branch) => {
                let mut child_rlp_pointers: [Option<&[u8]>; 16] = [None; 16];
                if let Some(fixed) = &branch.fixed {
                    for i in 0..16 {
                        if let Some(child) = &fixed.children[i] {
                            child_rlp_pointers[i] = Some(child.as_ref());
                        }
                    }
                }
                let mut dirty_children = dirty_children.iter();
                for (n, child) in &branch.changed_children {
                    let child = if let Some(child) = child {
                        child
                    } else {
                        child_rlp_pointers[*n as usize] = None;
                        continue;
                    };
                    child_rlp_pointers[*n as usize] = Some(
                        child
                            .rlp_pointer
                            .as_ref()
			    .or_else(|| dirty_children.next())
                            .map(|c| c.as_ref())
                            .expect("branch node rlp: child rlp must be computed or provided as dirty children arg"),
                    );
                }
                let len = encode_len_branch_node(&child_rlp_pointers);
                let mut out = Vec::with_capacity(len);
                encode_branch_node(&child_rlp_pointers, &mut out);
                out
            }
            DiffTrieNodeKind::Null => {
                let mut out = Vec::with_capacity(1);
                encode_null_node(&mut out);
                out
            }
        };
        Bytes::from(out)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DiffTrieNodeKind {
    Leaf(DiffLeafNode),
    Extension(DiffExtensionNode),
    Branch(DiffBranchNode),
    Null,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffLeafNode {
    pub fixed: Option<Arc<FixedLeafNode>>,
    pub changed_key: Option<Nibbles>,
    pub changed_value: Option<Bytes>,
}

impl DiffLeafNode {
    pub fn key(&self) -> &Nibbles {
        if let Some(changed) = &self.changed_key {
            return changed;
        }
        self.fixed
            .as_ref()
            .map(|k| &k.key)
            .expect("leaf incorrect form")
    }

    pub fn key_mut(&mut self) -> &mut Nibbles {
        if self.changed_key.is_none() {
            let fixed_key = self
                .fixed
                .as_ref()
                .map(|k| k.key.clone())
                .expect("leaf incorrect form");
            self.changed_key = Some(fixed_key);
        }
        self.changed_key.as_mut().unwrap()
    }

    pub fn value(&self) -> &Bytes {
        if let Some(changed) = &self.changed_value {
            return changed;
        }
        self.fixed
            .as_ref()
            .map(|k| &k.value)
            .expect("leaf incorrect form")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffChildPtr {
    pub rlp_pointer: Option<Bytes>,
    pub ptr: Option<u64>,
}

impl DiffChildPtr {
    pub fn new(ptr: u64) -> Self {
        Self {
            rlp_pointer: None,
            ptr: Some(ptr),
        }
    }

    pub fn ptr(&self) -> u64 {
        self.ptr.unwrap_or(u64::MAX)
    }

    pub fn mark_dirty(&mut self) {
        self.rlp_pointer = None;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffBranchNode {
    pub fixed: Option<Arc<FixedBranchNode>>,
    /// this must have an element for children that we have in the diff trie
    pub changed_children: SmallVec<[(u8, Option<DiffChildPtr>); 2]>,
    pub aux_bits: u16,
}

impl DiffBranchNode {
    pub fn has_child(&self, nibble: u8) -> bool {
        if let Some((_, changed_child)) = self.changed_children.iter().find(|(n, _)| n == &nibble) {
            return changed_child.is_some();
        }
        if let Some(fixed) = &self.fixed {
            return fixed.children[nibble as usize].is_some();
        }
        false
    }

    pub fn get_diff_child_mut(&mut self, nibble: u8) -> Option<&mut DiffChildPtr> {
        self.changed_children
            .iter_mut()
            .find(|(n, _)| n == &nibble)
            .and_then(|(_, c)| c.as_mut())
    }
    pub fn get_diff_child(&self, nibble: u8) -> Option<&DiffChildPtr> {
        self.changed_children
            .iter()
            .find(|(n, _)| n == &nibble)
            .and_then(|(_, c)| c.as_ref())
    }

    pub fn insert_diff_child(&mut self, nibble: u8, ptr: DiffChildPtr) {
        if let Some((_, child)) = self.changed_children.iter_mut().find(|(n, _)| n == &nibble) {
            *child = Some(ptr);
            return;
        }
        self.push_changed_children_sorted(nibble, Some(ptr));
    }

    pub fn child_count(&self) -> usize {
        // @perf try using mask in the branch node
        let mut count = 0;
        for i in 0..16 {
            if self.has_child(i) {
                count += 1;
            }
        }
        count
    }

    pub fn other_child_nibble(&self, child: u8) -> Option<u8> {
        for i in 0..16 {
            if i == child {
                continue;
            }
            if self.has_child(i) {
                return Some(i);
            }
        }
        None
    }

    pub fn other_child_ptr_and_nibble(&self, nibble: u8) -> Option<(u64, u8)> {
        for i in 0..16 {
            if i == nibble {
                continue;
            }
            if let Some(ptr) = self.get_diff_child(i) {
                return ptr.ptr.map(|p| (p, i));
            }
        }
        None
    }

    pub fn delete_child(&mut self, nibble: u8) {
        if let Some((_, child)) = self.changed_children.iter_mut().find(|(n, _)| n == &nibble) {
            *child = None;
            return;
        }
        self.push_changed_children_sorted(nibble, None);
    }

    fn push_changed_children_sorted(&mut self, nibble: u8, value: Option<DiffChildPtr>) {
        let pos = self.changed_children.iter().position(|(n, _)| n > &nibble);
        if let Some(pos) = pos {
            self.changed_children.insert(pos, (nibble, value));
        } else {
            self.changed_children.push((nibble, value));
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffExtensionNode {
    pub fixed: Option<Arc<FixedExtensionNode>>,
    pub changed_key: Option<Nibbles>,
    pub child: DiffChildPtr,
}

impl DiffExtensionNode {
    pub fn key(&self) -> &Nibbles {
        if let Some(changed) = &self.changed_key {
            return changed;
        }
        self.fixed
            .as_ref()
            .map(|k| &k.key)
            .expect("ext incorrect form")
    }

    pub fn key_mut(&mut self) -> &mut Nibbles {
        if self.changed_key.is_none() {
            let fixed_key = self
                .fixed
                .as_ref()
                .map(|k| k.key.clone())
                .expect("ext incorrect form");
            self.changed_key = Some(fixed_key);
        }
        self.changed_key.as_mut().unwrap()
    }
}
