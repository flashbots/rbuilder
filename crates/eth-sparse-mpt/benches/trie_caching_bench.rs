use alloy_primitives::{keccak256, Bytes, B256, U256};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use eth_sparse_mpt::{
    reth_sparse_trie::SparseTrieSharedCache,
    sparse_mpt::{DiffTrie, FixedTrie, FixedTrieNode},
    test_utils::{get_test_change_set, get_test_multiproofs},
};

fn prepare_values(n: usize, seed: usize) -> Vec<Bytes> {
    let mut values = Vec::with_capacity(n);
    for i in 0u64..(n as u64) {
        let b: B256 = U256::from(i + seed as u64).into();
        let value = keccak256(b).to_vec();
        values.push(Bytes::copy_from_slice(value.as_slice()));
    }
    values
}

fn generate_trie(size: usize) -> (FixedTrie, Vec<Bytes>) {
    let mut diff_trie = DiffTrie::new_empty();
    let keys = prepare_values(size, 0);
    let values = prepare_values(size, 1);
    for (key, value) in keys.clone().into_iter().zip(values.into_iter()) {
        diff_trie.insert(key, value).unwrap();
    }
    diff_trie.root_hash().unwrap();
    (FixedTrie::from_hashed_diff_trie_test(&diff_trie), keys)
}

fn root_hash_real_trie_cached(c: &mut Criterion) {
    let multiproof = get_test_multiproofs();
    let changes = get_test_change_set();
    const ACCOUNTS_TO_CHANGE: usize = 5;

    let shared_cache = SparseTrieSharedCache::default();
    for p in multiproof {
        shared_cache
            .update_cache_with_fetched_nodes(p)
            .expect("populate shared cache")
    }

    let mut fixed_trie = shared_cache.clone_inner().account_trie;
    let mut bytes_size = 0;
    for node in fixed_trie.nodes.values() {
	match node  {
	    FixedTrieNode::Leaf(node) => {
		bytes_size += node.key.len();
		bytes_size += node.value.len();
	    },
	    FixedTrieNode::Extension { node, child_ptr } => {
		bytes_size += node.key.len();
		bytes_size += node.child.len();
	    },
	    FixedTrieNode::Branch { node, child_ptrs } => {
		bytes_size += node.children.iter().map(|c| c.as_ref().map(|c| c.len()).unwrap_or(0)).sum::<usize>();
	    },
	    FixedTrieNode::Null => {},
	}
    }
    dbg!(bytes_size);
    let keys = changes.account_trie_updates;
    let mut values = prepare_values(keys.len(), 1231231);

    let mut trie = fixed_trie.gather_subtrie(&keys, &[]).unwrap();
    for (key, value) in keys.iter().zip(values.iter()) {
        trie.insert(key.clone(), value.clone())
            .expect("must insert");
    }
    trie.root_hash().unwrap();

    fixed_trie
        .changes_cache
        .update_from_trie_bredcrumbs(&trie.breadcrumbs, &trie);

    for i in 0..ACCOUNTS_TO_CHANGE {
        *values.get_mut(i).unwrap() = prepare_values(1, i + 123123112313).remove(0);
    }

    let zip_changes = keys
        .clone()
        .into_iter()
        .zip(values.clone())
        .collect::<Vec<_>>();

    let mut cached_hash = B256::ZERO;
    let mut non_cached_hash = B256::ZERO;
    c.bench_function("cache_real_account_trie_cached", |b| {
        b.iter_batched(
            || &fixed_trie,
            |fixed_trie| {
                let (mut diff_trie, cached_keys) = fixed_trie
                    .gather_subtrie_for_changes(&zip_changes, &[])
                    .unwrap();
                for (key, value) in &zip_changes {
                    if cached_keys.contains(key) {
                        continue;
                    }
                    diff_trie.insert(key.clone(), value.clone()).unwrap();
                }
                cached_hash = diff_trie.root_hash().unwrap();
            },
            BatchSize::SmallInput,
        );
    });

    c.bench_function("cache_real_account_trie_baseline", |b| {
        b.iter_batched(
            || &fixed_trie,
            |fixed_trie| {
                let mut diff_trie = fixed_trie.gather_subtrie(&keys, &[]).unwrap();
                for (key, value) in &zip_changes {
                    diff_trie.insert(key.clone(), value.clone()).unwrap();
                }
                non_cached_hash = diff_trie.root_hash().unwrap();
            },
            BatchSize::SmallInput,
        );
    });
    assert_eq!(cached_hash, non_cached_hash);
}

fn root_hash_cached(c: &mut Criterion) {
    const NUM_ELEMENTS: usize = 300000;
    const INSERTED_KEYS: usize = 500;

    let (fixed_trie, keys) = generate_trie(NUM_ELEMENTS);

    let keys = keys.into_iter().take(INSERTED_KEYS).collect::<Vec<_>>();
    let mut changed_values = prepare_values(INSERTED_KEYS, 1231231231231298);

    let mut diff_trie = fixed_trie.gather_subtrie(&keys, &[]).unwrap();
    let mut ctn = 0;
    for (key, value) in keys.iter().zip(changed_values.iter()) {
        ctn += 1;
        diff_trie.insert(key.clone(), value.clone()).unwrap();
    }
    diff_trie.root_hash().unwrap();
    let mut cached_trie = fixed_trie.clone();
    cached_trie
        .changes_cache
        .update_from_trie_bredcrumbs(&diff_trie.breadcrumbs, &diff_trie);
    println!(
        "cache size: {},  ctn: {}",
        cached_trie.changes_cache.cache.len(),
        ctn
    );

    *changed_values.last_mut().unwrap() = prepare_values(1, 1231231).remove(0);

    let zip_changes = keys
        .clone()
        .into_iter()
        .zip(changed_values.clone())
        .collect::<Vec<_>>();

    c.bench_function("cached_root_hash_cached", |b| {
        b.iter_batched(
            || &cached_trie,
            |fixed_trie| {
                let (mut diff_trie, cached_keys) = fixed_trie
                    .gather_subtrie_for_changes(&zip_changes, &[])
                    .unwrap();
                // dbg!(&diff_trie.len());
                // println!("ck: {}", cached_keys.len());
                for (key, value) in keys.iter().zip(changed_values.iter()) {
                    if cached_keys.contains(key) {
                        continue;
                    }
                    diff_trie.insert(key.clone(), value.clone()).unwrap();
                }
                diff_trie.root_hash().unwrap();
            },
            BatchSize::SmallInput,
        );
    });

    c.bench_function("cached_root_hash_baseline", |b| {
        b.iter_batched(
            || &fixed_trie,
            |fixed_trie| {
                let mut diff_trie = fixed_trie.gather_subtrie(&keys, &[]).unwrap();
                // dbg!(&diff_trie.len());
                for (key, value) in &zip_changes {
                    diff_trie.insert(key.clone(), value.clone()).unwrap();
                }
                diff_trie.root_hash().unwrap();
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, root_hash_real_trie_cached, root_hash_cached,);
criterion_main!(benches);
