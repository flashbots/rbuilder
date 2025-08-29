use std::collections::HashMap;

use alloy_primitives::{keccak256, Bytes, B256, U256};
use criterion::{criterion_group, criterion_main, Criterion};
use eth_sparse_mpt::{
    test_utils::{get_test_change_set, get_test_multiproofs},
    v1::sparse_mpt::{DiffTrie, FixedTrie},
    v2::trie::{proof_store::ProofStore, Trie},
};
use nybbles::Nibbles;

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

fn insert_nodes(c: &mut Criterion) {
    let empty_proof_store = ProofStore::default();
    let (keys, values) = prepare_key_value_data(10000);

    let mut trie = Trie::new_empty();
    let mut do_hash = B256::ZERO;
    c.bench_function("insert_nodes_do_trie", |b| {
        b.iter(|| {
            trie.clear_empty();
            for (key, value) in keys.iter().zip(values.iter()) {
                trie.insert(key, value).unwrap();
            }
            do_hash = trie.root_hash(true, &empty_proof_store).unwrap();
        })
    });

    let mut baseline_hash = B256::ZERO;
    let mut trie = DiffTrie::new_empty();
    c.bench_function("insert_nodes_basic_trie", |b| {
        b.iter(|| {
            trie.clear_empty();
            for (key, value) in keys.iter().zip(values.iter()) {
                trie.insert(key.clone(), value.clone()).unwrap();
            }
            baseline_hash = trie.root_hash_parallel().unwrap();
        })
    });
    if !do_hash.is_zero() && !baseline_hash.is_zero() {
        assert_eq!(do_hash, baseline_hash);
    }
}

fn insert_proofs(c: &mut Criterion) {
    let (byte_keys, byte_values) = {
        let change_set = get_test_change_set();
        let byte_keys = change_set.account_trie_updates;
        let (_, byte_values) = prepare_key_value_data(byte_keys.len());
        (byte_keys, byte_values)
    };

    let fixed_trie = {
        let mut trie = FixedTrie::default();
        for proof in get_test_multiproofs() {
            trie.add_nodes(&proof.account_subtree).unwrap();
        }
        trie
    };
    let mut baseline_hash = B256::ZERO;
    c.bench_function("gather_and_hash_basic_trie", |b| {
        b.iter(|| {
            let mut diff_trie = fixed_trie.gather_subtrie(&byte_keys, &[]).unwrap();
            for (key, value) in byte_keys.iter().zip(byte_values.iter()) {
                diff_trie.insert(key.clone(), value.clone()).unwrap();
            }
            baseline_hash = diff_trie.root_hash_parallel().unwrap();
        })
    });

    let proof_store = ProofStore::default();

    let mut nodes: HashMap<Nibbles, Vec<u8>> = Default::default();
    for proof in get_test_multiproofs() {
        for (path, node) in proof.account_subtree {
            nodes.insert(path, node.into());
        }
    }

    let mut proofs: HashMap<Nibbles, Vec<(Nibbles, Bytes)>> = Default::default();
    for key in &byte_keys {
        let key = Nibbles::unpack(key);
        let current_key_proofs = proofs.entry(key.clone()).or_default();
        for (path, node) in &nodes {
            if key.starts_with(path) {
                current_key_proofs.push((path.clone(), node.clone().into()));
            }
        }
        current_key_proofs.sort_by_key(|(p, _)| p.clone());
        current_key_proofs.dedup_by_key(|(p, _)| p.clone());
    }
    for (path, proof) in proofs {
        proof_store.add_proof(path, proof).unwrap();
    }

    let mut do_hash = B256::ZERO;
    let mut do_trie = Trie::new_empty();
    // let mut tmp = Vec::new();
    c.bench_function("gather_and_hash_do_trie", |b| {
        b.iter(|| {
            do_trie.clear();
            for (key, value) in byte_keys.iter().zip(byte_values.iter()) {
                let ok = do_trie
                    .try_add_proof_from_proof_store(&Nibbles::unpack(key), &proof_store)
                    .unwrap();
                assert!(ok);
                do_trie.insert(key, value).unwrap();
            }
            do_hash = do_trie.root_hash(true, &proof_store).unwrap();
        })
    });

    c.bench_function("gather_and_hash_do_trie_only_add_proof", |b| {
        b.iter(|| {
            do_trie.clear();
            for (key, _value) in byte_keys.iter().zip(byte_values.iter()) {
                // tmp.clear();
                // let ok = proof_store.try_get_proof(&Nibbles::unpack(key), &mut tmp);
                // assert!(ok);
                // do_trie.add_proof(&tmp).unwrap();
                let ok = do_trie
                    .try_add_proof_from_proof_store(&Nibbles::unpack(key), &proof_store)
                    .unwrap();
                assert!(ok);
            }
        })
    });

    if !do_hash.is_zero() && !baseline_hash.is_zero() {
        assert_eq!(do_hash, baseline_hash);
    }
}

criterion_group!(benches, insert_nodes, insert_proofs);
criterion_main!(benches);
