use std::hash::{Hash, Hasher};

use alloy_primitives::{keccak256, Bytes};
use alloy_rlp::{length_of_length, BufMut, Encodable, Header, EMPTY_STRING_CODE};
use alloy_trie::nodes::{ExtensionNodeRef, LeafNodeRef};
use nybbles::Nibbles;
use reth_trie::RlpNode;
use rustc_hash::{FxBuildHasher, FxHasher};

pub type HashMap<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;
pub type HashSet<K> = std::collections::HashSet<K, FxBuildHasher>;

pub fn hash_map_with_capacity<K, V>(capacity: usize) -> HashMap<K, V> {
    HashMap::with_capacity_and_hasher(capacity, FxBuildHasher)
}

pub fn fast_hash<H: Hash>(value: &H) -> u64 {
    let mut hasher = FxHasher::default();
    value.hash(&mut hasher);
    hasher.finish()
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

#[inline]
pub fn extract_prefix_and_suffix(p1: &Nibbles, p2: &Nibbles) -> (Nibbles, Nibbles, Nibbles) {
    let prefix_len = p1.common_prefix_length(p2);
    let prefix = Nibbles::from_nibbles_unchecked(&p1[..prefix_len]);
    let suffix1 = Nibbles::from_nibbles_unchecked(&p1[prefix_len..]);
    let suffix2 = Nibbles::from_nibbles_unchecked(&p2[prefix_len..]);

    (prefix, suffix1, suffix2)
}

#[inline]
pub fn encode_leaf(key: &Nibbles, value: &[u8], out: &mut dyn BufMut) {
    LeafNodeRef { key, value }.encode(out)
}

pub fn encode_len_leaf(key: &Nibbles, value: &[u8]) -> usize {
    LeafNodeRef { key, value }.length()
}

#[inline]
pub fn encode_extension(key: &Nibbles, child_rlp_pointer: &[u8], out: &mut dyn BufMut) {
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

#[inline]
pub fn encode_branch_node(child_rlp_pointers: &[Option<&[u8]>; 16], out: &mut dyn BufMut) {
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

#[inline]
pub fn mismatch(xs: &[u8], ys: &[u8]) -> usize {
    mismatch_chunks::<8>(xs, ys)
}

#[inline]
fn mismatch_chunks<const N: usize>(xs: &[u8], ys: &[u8]) -> usize {
    let off = std::iter::zip(xs.chunks_exact(N), ys.chunks_exact(N))
        .take_while(|(x, y)| x == y)
        .count()
        * N;
    off + std::iter::zip(&xs[off..], &ys[off..])
        .take_while(|(x, y)| x == y)
        .count()
}

// rbuilder uses nybbles v3.3.0 and reth_trie uses nybbles v4.3.0. This is a temporary fix to convert between the two.
// We can remove the below methods once rbuilder has been upgraded to nybbles v4.3.0.
// nybbles v4.3.0 has a breaking change (byte array with 1 byte per nybble vs U256 packed data + length) in the API which breaks a lot of parts of the eth sparse trie code.
#[inline]
pub fn convert_reth_nybbles_to_nibbles(n: reth_trie::Nibbles) -> Nibbles {
    Nibbles::from_nibbles(n.to_vec())
}

#[inline]
pub fn convert_nibbles_to_reth_nybbles(n: Nibbles) -> reth_trie::Nibbles {
    reth_trie::Nibbles::from_nibbles(n.as_slice())
}
