use alloy_primitives::{keccak256, Bytes, B256, U256};
use criterion::{criterion_group, criterion_main, Criterion};
use eth_sparse_mpt::{
    v2::trie::{proof_store::ProofStore as ProofStoreV2, Trie as TrieV2},
    v3::trie::{proof_store::ProofStore as ProofStoreV3, Trie as TrieV3},
};

fn prepare_key_value_data(n: usize) -> (Vec<Bytes>, Vec<Bytes>) {
    let mut keys = Vec::with_capacity(n);
    let mut values = Vec::with_capacity(n);
    for i in 0u64..(n as u64) {
        let b: B256 = U256::from(i).into();
        let data = keccak256(b).to_vec();
        let value = keccak256(&data).to_vec();
        keys.push(Bytes::copy_from_slice(data.as_slice()));
        values.push(Bytes::copy_from_slice(value.as_slice()));
    }
    (keys, values)
}

fn insert_nodes_v2_v3(c: &mut Criterion) {
    let (keys, values) = prepare_key_value_data(10000);

    let empty_proof_store_v2 = ProofStoreV2::default();
    let mut v2_hash = B256::ZERO;
    let mut trie_v2 = TrieV2::new_empty();
    c.bench_function("insert_nodes_v2", |b| {
        b.iter(|| {
            trie_v2.clear_empty();
            for (key, value) in keys.iter().zip(values.iter()) {
                trie_v2.insert(key, value).unwrap();
            }
            v2_hash = trie_v2.root_hash(true, &empty_proof_store_v2).unwrap();
        })
    });

    let empty_proof_store_v3 = ProofStoreV3::default();
    let mut v3_hash = B256::ZERO;
    let mut trie_v3 = TrieV3::new_empty();
    c.bench_function("insert_nodes_v3", |b| {
        b.iter(|| {
            trie_v3.clear_empty();
            for (key, value) in keys.iter().zip(values.iter()) {
                trie_v3.insert(key, value).unwrap();
            }
            v3_hash = trie_v3.root_hash(true, &empty_proof_store_v3).unwrap();
        })
    });

    if !v2_hash.is_zero() && !v3_hash.is_zero() {
        assert_eq!(v2_hash, v3_hash);
    }
}

criterion_group!(benches, insert_nodes_v2_v3);
criterion_main!(benches);
