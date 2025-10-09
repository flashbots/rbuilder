use alloy_primitives::{hex_literal::hex, Bytes};
use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use eth_sparse_mpt::{
    test_utils::{get_test_change_set, get_test_multiproofs},
    v1::{
        reth_sparse_trie::{
            change_set::ETHTrieChangeSet, hash::EthSparseTries,
            shared_cache::RethSparseTrieShareCacheInternal, SparseTrieSharedCache,
        },
        sparse_mpt::{DiffTrie, FixedTrie},
    },
};

fn get_storage_tries(changes: &ETHTrieChangeSet, tries: &EthSparseTries) -> Vec<DiffTrie> {
    let mut storage_tries = Vec::new();
    for account in changes.account_trie_updates.iter() {
        storage_tries.push(
            tries
                .storage_tries
                .get(account)
                .expect("storage trie must exist")
                .clone(),
        );
    }
    storage_tries
}

fn apply_storage_tries_changes<'a>(
    storage_tries: impl Iterator<Item = &'a mut DiffTrie>,
    changes: &ETHTrieChangeSet,
) {
    for (i, trie) in storage_tries.enumerate() {
        let keys = &changes.storage_trie_updated_keys[i];
        let value = &changes.storage_trie_updated_values[i];
        let deletes = &changes.storage_trie_deleted_keys[i];
        for (k, v) in keys.iter().zip(value) {
            trie.insert(k.clone(), v.clone())
                .expect("must insert storage trie");
        }
        for d in deletes {
            trie.delete(d.clone()).expect("must delete storage trie");
        }
    }
}

fn gather_nodes(c: &mut Criterion) {
    let multiproof = get_test_multiproofs();
    let changes = get_test_change_set();

    let shared_cache = SparseTrieSharedCache::default();
    for p in multiproof {
        shared_cache
            .update_cache_with_fetched_nodes(p)
            .expect("populate shared cache")
    }

    let tries = shared_cache.gather_tries_for_changes(&changes).unwrap();
    println!("acc trie len: {}", tries.account_trie.len(),);

    c.bench_function("gather_nodes_clone", |b| {
        b.iter(|| {
            let out = tries.clone();
            black_box(out);
        })
    });

    c.bench_function("gather_nodes_shared_cache", |b| {
        b.iter(|| {
            let out = shared_cache
                .gather_tries_for_changes(&changes)
                .expect("gather must succeed");
            black_box(out);
        })
    });

    let internal_cache = shared_cache.clone_inner();
    let account: Bytes =
        hex!("61845bd4bf1d79174d0ba40156aea4c8aaded050ca7c00ce13d43878cd13a79d").into();
    let mut storage_trie_data = get_data_for_storage_trie(&internal_cache, &changes, &account);
    c.bench_function("gather_nodes_empty_account", |b| {
        b.iter(|| {
            let StorageTrieData {
                fixed_trie,
                updated_keys,
                deletes,
                ..
            } = &mut storage_trie_data;
            let out = fixed_trie
                .gather_subtrie(updated_keys, deletes)
                .expect("must gather");
            // dbg!(updated_keys, deletes, &out);
            // panic!();
            black_box(out);
        })
    });

    let account: Bytes =
        hex!("ab14d68802a763f7db875346d03fbf86f137de55814b191c069e721f47474733").into();
    let mut storage_trie_data = get_data_for_storage_trie(&internal_cache, &changes, &account);
    c.bench_function("gather_nodes_big_changes_account", |b| {
        b.iter(|| {
            let StorageTrieData {
                fixed_trie,
                updated_keys,
                deletes,
                ..
            } = &mut storage_trie_data;
            let out = fixed_trie
                .gather_subtrie(updated_keys, deletes)
                .expect("must gather");
            // dbg!(updated_keys, deletes, &out);
            // panic!();
            black_box(out);
        })
    });

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

    c.bench_function("gather_nodes_account_trie", |b| {
        b.iter(|| {
            let out = fixed_trie
                .gather_subtrie(&changes.account_trie_updates, &changes.account_trie_deletes)
                .expect("must gather");
            black_box(out);
        })
    });

    let inner_cache = shared_cache.clone_inner();
    c.bench_function("gather_nodes_storage_tries", |b| {
        b.iter(|| {
            for acc_idx in 0..changes.account_trie_updates.len() {
                // let start = std::time::Instant::now();
                let account = &changes.account_trie_updates[acc_idx];
                let updates = &changes.storage_trie_updated_keys[acc_idx];
                let deletes = &changes.storage_trie_deleted_keys[acc_idx];
                let storage_trie = inner_cache.storage_tries.get(account).expect("must exist");
                storage_trie
                    .gather_subtrie(updates, deletes)
                    .expect("must gather");
            }
        })
    });
}

fn root_hash_all(c: &mut Criterion) {
    let multiproof = get_test_multiproofs();
    let changes = get_test_change_set();

    let shared_cache = SparseTrieSharedCache::default();
    for p in multiproof {
        shared_cache
            .update_cache_with_fetched_nodes(p)
            .expect("populate shared cache")
    }

    let tries = shared_cache
        .gather_tries_for_changes(&changes)
        .expect("gather must succeed");

    c.bench_function("root_hash_all_par_all", |b| {
        b.iter_batched(
            || (tries.clone(), changes.clone()),
            |(mut tries, changes)| {
                tries
                    .calculate_root_hash(changes, true, true)
                    .expect("must hash")
            },
            BatchSize::SmallInput,
        );
    });
    c.bench_function("root_hash_all_no_par", |b| {
        b.iter_batched(
            || (tries.clone(), changes.clone()),
            |(mut tries, changes)| {
                tries
                    .calculate_root_hash(changes, false, false)
                    .expect("must hash")
            },
            BatchSize::SmallInput,
        );
    });

    c.bench_function("root_hash_all_par_storage", |b| {
        b.iter_batched(
            || (tries.clone(), changes.clone()),
            |(mut tries, changes)| {
                tries
                    .calculate_root_hash(changes, true, false)
                    .expect("must hash")
            },
            BatchSize::SmallInput,
        );
    });

    c.bench_function("root_hash_all_par_accounts", |b| {
        b.iter_batched(
            || (tries.clone(), changes.clone()),
            |(mut tries, changes)| {
                tries
                    .calculate_root_hash(changes, false, true)
                    .expect("must hash")
            },
            BatchSize::SmallInput,
        );
    });
}

fn root_hash_main_trie(c: &mut Criterion) {
    let multiproof = get_test_multiproofs();
    let changes = get_test_change_set();

    let shared_cache = SparseTrieSharedCache::default();
    for p in multiproof {
        shared_cache
            .update_cache_with_fetched_nodes(p)
            .expect("populate shared cache")
    }

    let mut trie = shared_cache
        .gather_tries_for_changes(&changes)
        .expect("gather must succeed")
        .account_trie;
    for key in changes.account_trie_updates {
        trie.insert(key.clone(), key.clone()).expect("must insert");
    }
    for key in changes.account_trie_deletes {
        trie.delete(key).expect("must update");
    }

    c.bench_function("root_hash_account_trie_no_cache", |b| {
        b.iter_batched(
            || trie.clone(),
            |mut trie| trie.root_hash().expect("must hash"),
            BatchSize::SmallInput,
        );
    });

    c.bench_function("root_hash_account_trie_parallel", |b| {
        b.iter_batched(
            || trie.clone(),
            |mut trie| trie.root_hash_parallel().expect("must hash"),
            BatchSize::SmallInput,
        );
    });
}

fn root_hash_storage(c: &mut Criterion) {
    let multiproof = get_test_multiproofs();
    let changes = get_test_change_set();

    let shared_cache = SparseTrieSharedCache::default();
    for p in multiproof {
        shared_cache
            .update_cache_with_fetched_nodes(p)
            .expect("populate shared cache")
    }

    let tries = shared_cache
        .gather_tries_for_changes(&changes)
        .expect("gather must succeed");

    c.bench_function("root_hash_storage_insert", |b| {
        b.iter_batched(
            || get_storage_tries(&changes, &tries),
            |mut storage_tries| {
                apply_storage_tries_changes(storage_tries.iter_mut(), &changes);
            },
            BatchSize::SmallInput,
        );
    });

    c.bench_function("root_hash_storage_hash", |b| {
        b.iter_batched(
            || get_storage_tries(&changes, &tries),
            |mut storage_tries| {
                apply_storage_tries_changes(storage_tries.iter_mut(), &changes);
                for trie in storage_tries.iter_mut() {
                    trie.root_hash().expect("must hash storage trie");
                }
            },
            BatchSize::SmallInput,
        );
    });
}

#[derive(Debug)]
struct StorageTrieData {
    fixed_trie: FixedTrie,
    updated_keys: Vec<Bytes>,
    // updated_values: Vec<Bytes>,
    deletes: Vec<Bytes>,
}

fn get_data_for_storage_trie(
    cache: &RethSparseTrieShareCacheInternal,
    change_set: &ETHTrieChangeSet,
    account: &Bytes,
) -> StorageTrieData {
    let acc_idx = change_set
        .account_trie_updates
        .iter()
        .position(|el| el == account)
        .expect("account not found");

    let updated_keys = change_set.storage_trie_updated_keys[acc_idx].clone();
    // let updated_values = change_set.storage_trie_updated_values[acc_idx].clone();
    let deletes = change_set.storage_trie_deleted_keys[acc_idx].clone();
    let fixed_trie = cache
        .storage_tries
        .get(account)
        .cloned()
        .unwrap_or_default();

    StorageTrieData {
        fixed_trie,
        updated_keys,
        // updated_values,
        deletes,
    }
}

criterion_group!(
    benches,
    gather_nodes,
    root_hash_all,
    root_hash_main_trie,
    root_hash_storage
);
criterion_main!(benches);
