use alloy_primitives::{keccak256, Bytes, B256, U256};
use alloy_rlp::Encodable;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use eth_sparse_mpt::{test_utils::KeccakHasher, utils::HashMap, v1::sparse_mpt::DiffTrie};

// hashing this trie it roughly equivalent to updating the trie for the block
const TRIE_SIZE: usize = 3000;

fn prepare_key_value_data(n: usize) -> (Vec<Bytes>, Vec<Bytes>) {
    let mut keys = Vec::with_capacity(n);
    let mut values = Vec::with_capacity(n);
    for i in 0u64..3000 {
        let b: B256 = U256::from(i).into();
        let data = keccak256(b).to_vec();
        let value = keccak256(&data).to_vec();
        keys.push(Bytes::copy_from_slice(data.as_slice()));
        values.push(Bytes::copy_from_slice(value.as_slice()));
    }
    (keys, values)
}

fn add_elements_bytes(keys: &[Bytes], values: &[Bytes]) -> B256 {
    triehash::trie_root::<KeccakHasher, _, _, _>(keys.iter().zip(values))
}

fn add_elements_only_neo_sparse_trie(keys: &[Bytes], values: &[Bytes]) {
    let mut trie = DiffTrie::new_empty();
    // trie.reserve(keys.len());
    for (key, value) in keys.iter().zip(values.iter()) {
        trie.insert(key.clone(), value.clone())
            .expect("can't insert");
    }
}

fn add_elements_only_neo_sparse_trie_insert_and_hash(keys: &[Bytes], values: &[Bytes]) -> B256 {
    let mut trie = DiffTrie::new_empty();
    for (key, value) in keys.iter().zip(values.iter()) {
        trie.insert(key.clone(), value.clone())
            .expect("can't insert");
    }
    trie.root_hash().expect("must hash")
}

fn ptr_trie_insert_only(c: &mut Criterion) {
    let (keys, values) = prepare_key_value_data(TRIE_SIZE);
    c.bench_function(&format!("ptr_trie_insert_only_{}", TRIE_SIZE), |b| {
        b.iter(|| add_elements_only_neo_sparse_trie(&keys, &values))
    });
}

fn ptr_trie_insert_and_hash(c: &mut Criterion) {
    let (keys, values) = prepare_key_value_data(TRIE_SIZE);
    c.bench_function(&format!("ptr_trie_insert_and_hash_{}", TRIE_SIZE), |b| {
        b.iter(|| add_elements_only_neo_sparse_trie_insert_and_hash(&keys, &values))
    });
    c.bench_function(
        &format!("reference_trie_insert_and_hash_{}", TRIE_SIZE),
        |b| b.iter(|| add_elements_bytes(&keys, &values)),
    );
}

fn hashing(c: &mut Criterion) {
    let mut data = Vec::new();
    for _ in 0..TRIE_SIZE {
        data.push(B256::random());
    }

    let mut hash_cache = HashMap::default();

    c.bench_function(&format!("hashing_{}_elements", TRIE_SIZE), |b| {
        b.iter(|| {
            for d in data.iter() {
                let hash = keccak256(d);
                black_box(hash);
            }
        })
    });

    c.bench_function(&format!("hashing_{}_elements_with_cache", TRIE_SIZE), |b| {
        b.iter(|| {
            for d in data.iter() {
                let hash = hash_cache.entry(d).or_insert_with(|| keccak256(d));
                black_box(hash);
            }
        })
    });
}

fn cloning(c: &mut Criterion) {
    let mut data = Vec::new();
    for _ in 0..TRIE_SIZE {
        data.push(vec![B256::random(); 16]);
    }

    c.bench_function(
        &format!("cloning_{}_branch_node_size_elements", TRIE_SIZE),
        |b| {
            b.iter(|| {
                black_box(data.clone());
            })
        },
    );

    c.bench_function(
        &format!("hashing_{}_branch_node_size_elements", TRIE_SIZE),
        |b| {
            let mut buff = Vec::new();
            b.iter(|| {
                data.encode(&mut buff);
                black_box(keccak256(&buff));
            })
        },
    );
}

criterion_group!(
    benches,
    hashing,
    cloning,
    ptr_trie_insert_only,
    ptr_trie_insert_and_hash,
);
criterion_main!(benches);
