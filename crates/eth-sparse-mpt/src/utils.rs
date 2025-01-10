use alloy_primitives::{keccak256, Bytes, B256};
use alloy_rlp::{length_of_length, BufMut, Encodable, Header, EMPTY_STRING_CODE};
use alloy_trie::{
    nodes::{ExtensionNodeRef, LeafNodeRef},
    Nibbles,
};
use reth_trie::RlpNode;
use rustc_hash::{FxBuildHasher, FxHasher};
use serde::{Deserialize, Serialize};

use crate::{
    reth_sparse_trie::{change_set::ETHTrieChangeSet, trie_fetcher::MultiProof},
    sparse_mpt::DiffTrie,
};

pub type HashMap<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;
pub type HashSet<K> = std::collections::HashSet<K, FxBuildHasher>;

pub fn hash_map_with_capacity<K, V>(capacity: usize) -> HashMap<K, V> {
    HashMap::with_capacity_and_hasher(capacity, FxBuildHasher)
}

pub fn rlp_pointer(rlp_encode: Bytes) -> Bytes {
    if rlp_encode.len() < 32 {
        rlp_encode
    } else {
        Bytes::copy_from_slice(RlpNode::word_rlp(&keccak256(&rlp_encode)).as_ref())
    }
}

pub fn concat_path(p1: &Nibbles, p2: &[u8]) -> Nibbles {
    let mut result = Nibbles::with_capacity(p1.len() + p2.len());
    result.extend_from_slice_unchecked(p1);
    result.extend_from_slice_unchecked(p2);
    result
}

pub fn strip_first_nibble_mut(p: &mut Nibbles) -> u8 {
    let nibble = p[0];
    let vec = p.as_mut_vec_unchecked();
    vec.remove(0);
    nibble
}

pub fn extract_prefix_and_suffix(p1: &Nibbles, p2: &Nibbles) -> (Nibbles, Nibbles, Nibbles) {
    let prefix_len = p1.common_prefix_length(p2);
    let prefix = Nibbles::from_nibbles_unchecked(&p1[..prefix_len]);
    let suffix1 = Nibbles::from_nibbles_unchecked(&p1[prefix_len..]);
    let suffix2 = Nibbles::from_nibbles_unchecked(&p2[prefix_len..]);

    (prefix, suffix1, suffix2)
}

pub fn encode_leaf(key: &Nibbles, value: &[u8], out: &mut Vec<u8>) {
    LeafNodeRef { key, value }.encode(out)
}

pub fn encode_len_leaf(key: &Nibbles, value: &[u8]) -> usize {
    LeafNodeRef { key, value }.length()
}

pub fn encode_extension(key: &Nibbles, child_rlp_pointer: &[u8], out: &mut Vec<u8>) {
    ExtensionNodeRef {
        key,
        child: child_rlp_pointer,
    }
    .encode(out)
}

pub fn encode_len_extension(key: &Nibbles, child_rlp_pointer: &[u8]) -> usize {
    ExtensionNodeRef {
        key,
        child: child_rlp_pointer,
    }
    .length()
}

pub fn encode_branch_node(child_rlp_pointers: &[Option<&[u8]>; 16], out: &mut Vec<u8>) {
    let mut payload_length = 1;
    for i in 0..16 {
        if let Some(child) = child_rlp_pointers[i] {
            payload_length += child.len();
        } else {
            payload_length += 1;
        }
    }

    Header {
        list: true,
        payload_length,
    }
    .encode(out);

    for i in 0..16 {
        if let Some(child) = child_rlp_pointers[i] {
            out.put_slice(child);
        } else {
            out.put_u8(EMPTY_STRING_CODE);
        }
    }
    out.put_u8(EMPTY_STRING_CODE);
}

pub fn encode_len_branch_node(child_rlp_pointers: &[Option<&[u8]>; 16]) -> usize {
    let mut payload_length = 1;
    for i in 0..16 {
        if let Some(child) = child_rlp_pointers[i] {
            payload_length += child.len();
        } else {
            payload_length += 1;
        }
    }
    payload_length + length_of_length(payload_length)
}

pub fn encode_null_node(out: &mut Vec<u8>) {
    out.push(EMPTY_STRING_CODE)
}

// test utils

#[derive(Debug)]
pub struct KeccakHasher {}

impl hash_db::Hasher for KeccakHasher {
    type Out = B256;
    type StdHasher = FxHasher;
    const LENGTH: usize = 32;

    fn hash(x: &[u8]) -> Self::Out {
        keccak256(x)
    }
}

pub fn reference_trie_hash(data: &[(Bytes, Bytes)]) -> B256 {
    triehash::trie_root::<KeccakHasher, _, _, _>(data.to_vec())
}

pub fn get_test_multiproofs() -> Vec<MultiProof> {
    let files = [
        "./test_data/multiproof_0.json",
        "./test_data/multiproof_1.json",
    ];
    let mut result = Vec::new();
    for file in files {
        let data = std::fs::read_to_string(file).expect("reading multiproof");
        result.push(serde_json::from_str(&data).expect("parsing multiproof"));
    }
    result
}

pub fn get_test_change_set() -> ETHTrieChangeSet {
    let data = std::fs::read_to_string("./test_data/changeset.json").expect("reading changeset");
    serde_json::from_str(&data).expect("parsing changeset")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredFailureCase {
    pub trie: DiffTrie,
    pub updated_keys: Vec<Bytes>,
    pub updated_values: Vec<Bytes>,
    pub deleted_keys: Vec<Bytes>,
}

impl StoredFailureCase {
    pub fn load(path: &str) -> StoredFailureCase {
        // raed from file
        let data = std::fs::read_to_string(path).expect("reading stored failure case");
        serde_json::from_str(&data).expect("parsing stored failure case")
    }
}
