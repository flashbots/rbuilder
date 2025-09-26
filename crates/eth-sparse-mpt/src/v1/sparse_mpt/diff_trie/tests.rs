use super::*;
use crate::{
    test_utils::{reference_trie_hash, StoredFailureCase},
    utils::HashSet,
    v1::sparse_mpt::*,
};
use alloy_primitives::{Bytes, B256};
use eyre::Context;
use proptest::prelude::*;
use rand::{seq::SliceRandom, SeedableRng};

fn convert_input_to_bytes(input: &[(Vec<u8>, Vec<u8>)]) -> Vec<(Bytes, Bytes)> {
    input
        .iter()
        .map(|(k, v)| (k.clone().into(), v.clone().into()))
        .collect()
}

fn compare_impls_with_hashing(data: Vec<(Bytes, Bytes)>, insert_hashing: bool) {
    let expected = reference_trie_hash(&data);
    let mut trie = DiffTrie::new_empty();
    for (key, value) in data {
        trie.insert(key, value).expect("can't insert");
        if insert_hashing {
            trie.root_hash().expect("must hash");
        }
    }
    let got = trie.root_hash().expect("hashing failed");
    assert_eq!(
        got, expected,
        "comparing hashing, insert_hashing: {insert_hashing}",
    );

    let got_parallel = trie.root_hash_parallel().expect("hashing failed");
    assert_eq!(
        got_parallel, expected,
        "comparing parallel hashing, insert_hashing: {insert_hashing}",
    );
}

fn compare_sparse_impl(mut data: Vec<(Bytes, Bytes)>, insert_hashing: bool) {
    let expected = reference_trie_hash(&data);

    let (last, data) = if let Some(last) = data.pop() {
        (vec![last], data)
    } else {
        (vec![], data)
    };

    let mut trie = DiffTrie::new_empty();
    for (key, value) in data {
        trie.insert(key, value).expect("can't insert");
        if insert_hashing {
            trie.root_hash().expect("must hash");
        }
    }
    trie.root_hash().expect("must hash");

    let fixed_trie = FixedTrie::from_hashed_diff_trie_test(&trie);

    let changed_keys = last.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
    let mut gathered_trie = fixed_trie
        .gather_subtrie(&changed_keys, &[])
        .expect("gather must work");
    for (k, v) in last {
        gathered_trie
            .insert(k, v)
            .expect("can't insert into gathered trie");
    }
    let got = gathered_trie.root_hash().expect("can't hash gathered trie");
    assert_eq!(got, expected);

    let got_parallel = gathered_trie
        .root_hash_parallel()
        .expect("can't hash gathered trie");
    assert_eq!(got_parallel, expected);
}

fn compare_impls(data: &[(Vec<u8>, Vec<u8>)]) {
    let data = convert_input_to_bytes(data);
    compare_impls_with_hashing(data.clone(), false);
    compare_impls_with_hashing(data.clone(), true);
    compare_sparse_impl(data.clone(), false);
    compare_sparse_impl(data.clone(), true);
}

#[test]
fn empty_trie() {
    compare_impls(&[])
}

#[test]
fn one_element_trie() {
    let data = [(vec![1, 1], vec![0xa, 0xa])];
    compare_impls(&data)
}

#[test]
fn update_leaf_node() {
    let data = &[(vec![1], vec![2]), (vec![1], vec![3])];
    compare_impls(data);
}

#[test]
fn insert_into_leaf_node_no_extension() {
    let data = &[(vec![0x11], vec![0x0a]), (vec![0x22], vec![0x0b])];
    compare_impls(data);

    let data = &[(vec![0x22], vec![0x0b]), (vec![0x11], vec![0x0a])];
    compare_impls(data);
}
#[test]
fn insert_into_leaf_node_with_extension() {
    let data = &[
        (vec![0x33, 0x22], vec![0x0a]),
        (vec![0x33, 0x11], vec![0x0b]),
    ];
    compare_impls(data);
}

#[test]
fn insert_into_extension_node_no_extension_above() {
    let data = &[
        (vec![0x33, 0x22], vec![0x0a]),
        (vec![0x33, 0x11], vec![0x0b]),
        (vec![0x44, 0x33], vec![0x0c]),
    ];
    compare_impls(data);
}

#[test]
fn insert_into_extension_node_with_extension_above() {
    let data = &[
        (vec![0x33, 0x33, 0x22], vec![0x0a]),
        (vec![0x33, 0x33, 0x11], vec![0x0b]),
        (vec![0x33, 0x44, 0x33], vec![0x0c]),
    ];
    compare_impls(data);
}

#[test]
fn insert_into_extension_node_collapse_extension() {
    let data = &[
        (vec![0x33, 0x22, 0x44], vec![0x0a]),
        (vec![0x33, 0x11, 0x44], vec![0x0b]),
        (vec![0x34, 0x33, 0x44], vec![0x0c]),
    ];
    compare_impls(data);
}

#[test]
fn insert_into_extension_node_collapse_extension_no_ext_above() {
    let data = &[
        (vec![0x31, 0x11], vec![0x0a]),
        (vec![0x32, 0x22], vec![0x0b]),
        (vec![0x11, 0x33], vec![0x0c]),
    ];
    compare_impls(data);
}

#[test]
fn insert_into_branch_empty_child() {
    let data = &[
        (vec![0x11], vec![0x0a]),
        (vec![0x22], vec![0x0b]),
        (vec![0x33], vec![0x0c]),
    ];
    compare_impls(data);
}

#[test]
fn insert_into_branch_leaf_child() {
    let data = &[
        (vec![0x11], vec![0x0a]),
        (vec![0x22], vec![0x0b]),
        (vec![0x33], vec![0x0c]),
        (vec![0x33], vec![0x0d]),
    ];
    compare_impls(data);
}

fn reference_hash_with_removals(data: &[(Bytes, Bytes)], remove: &[Bytes]) -> B256 {
    let removed_keys: HashSet<_> = remove.iter().cloned().collect();
    let filtered_data: Vec<_> = data
        .iter()
        .filter(|(k, _)| !removed_keys.contains(k))
        .cloned()
        .collect();
    reference_trie_hash(&filtered_data)
}

fn compare_with_removals_with_hashing(
    data: Vec<(Bytes, Bytes)>,
    remove: Vec<Bytes>,
    insert_hashing: bool,
) -> Result<(), DeletionError> {
    let reference_hash = reference_hash_with_removals(&data, &remove);

    let mut trie = DiffTrie::new_empty();
    for (key, val) in data {
        trie.insert(key, val).expect("must insert");
        if insert_hashing {
            trie.root_hash().expect("must hash");
        }
    }

    for key in remove {
        trie.delete(key)?;
        if insert_hashing {
            trie.root_hash().expect("must hash");
        }
    }

    let hash = trie.root_hash().expect("must hash");
    assert_eq!(
        hash, reference_hash,
        "comparing hashing, insert_hashing: {insert_hashing}",
    );

    let parallel_hash = trie.root_hash_parallel().expect("must hash");
    assert_eq!(
        parallel_hash, reference_hash,
        "comparing parallel hashing, insert_hashing: {insert_hashing}",
    );

    Ok(())
}

fn compare_with_removals_sparse(
    data: Vec<(Bytes, Bytes)>,
    remove: Vec<Bytes>,
    insert_hashing: bool,
) -> Result<(), DeletionError> {
    let reference_hash = reference_hash_with_removals(&data, &remove);

    let mut trie = DiffTrie::new_empty();
    for (key, val) in data {
        trie.insert(key, val).expect("must insert");
        if insert_hashing {
            trie.root_hash().expect("must hash");
        }
    }
    trie.root_hash().expect("must hash");

    let fixed_trie = FixedTrie::from_hashed_diff_trie_test(&trie);
    let mut gathered_trie = fixed_trie
        .gather_subtrie(&[], &remove)
        .expect("gather must work");

    for key in remove {
        gathered_trie.delete(key)?;
        if insert_hashing {
            gathered_trie.root_hash().expect("must hash");
        }
    }

    let hash = gathered_trie.root_hash().expect("must hash");
    assert_eq!(
        hash, reference_hash,
        "comparing sparse removals, insert_hashing: {insert_hashing}",
    );

    let parallel_hash = gathered_trie.root_hash_parallel().expect("must hash");
    assert_eq!(
        parallel_hash, reference_hash,
        "comparing sparse removals parallel hashing, insert_hashing: {insert_hashing}",
    );

    Ok(())
}

fn compare_with_removals(data: &[(Vec<u8>, Vec<u8>)], remove: &[Vec<u8>]) -> eyre::Result<()> {
    let data = convert_input_to_bytes(data);
    let remove: Vec<Bytes> = remove.iter().map(|r| r.clone().into()).collect();
    compare_with_removals_with_hashing(data.clone(), remove.clone(), false)
        .with_context(|| "normal hashing: false")?;
    compare_with_removals_with_hashing(data.clone(), remove.clone(), true)
        .with_context(|| "normal hashing: true")?;
    compare_with_removals_sparse(data.clone(), remove.clone(), false)
        .with_context(|| "sparse hashing: false")?;
    compare_with_removals_sparse(data.clone(), remove.clone(), true)
        .with_context(|| "sparse hashing: true")?;
    Ok(())
}

#[test]
fn remove_empty_trie_err() {
    let add = &[];

    let remove = &[vec![0x12]];

    let _ = compare_with_removals(add, remove).unwrap_err();
}

#[test]
fn remove_leaf() {
    let add = &[(vec![0x11], vec![0x0a])];

    let remove = &[vec![0x11]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn remove_leaf_key_error() {
    let add = &[(vec![0x11], vec![0x0a])];

    let remove = &[vec![0x12]];

    let _ = compare_with_removals(add, remove).unwrap_err();
}

#[test]
fn remove_extension_node_error() {
    let add = &[(vec![0x11, 0x1], vec![0x0a]), (vec![0x11, 0x2], vec![0x0b])];

    let remove = &[vec![0x12]];

    let _ = compare_with_removals(add, remove).unwrap_err();
}

#[test]
fn remove_branch_err() {
    let add = &[
        (vec![0x01, 0x10], vec![0x0a]),
        (vec![0x01, 0x20], vec![0x0b]),
        (vec![0x01, 0x30], vec![0x0c]),
    ];

    let remove = &[vec![0x01]];

    let _ = compare_with_removals(add, remove).unwrap_err();
}

#[test]
fn remove_branch_leave_2_children() {
    let add = &[
        (vec![0x01], vec![0x0a]),
        (vec![0x02], vec![0x0b]),
        (vec![0x03], vec![0x0c]),
    ];

    let remove = &[vec![0x01]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn remove_branch_leave_1_children_leaf_below_branch_above() {
    let add = &[
        (vec![0x11], vec![0x0a]),
        (vec![0x12], vec![0x0b]),
        (vec![0x23], vec![0x0b]),
        (vec![0x33], vec![0x0c]),
    ];

    let remove = &[vec![0x11]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn remove_branch_leave_1_children_branch_below_branch_above() {
    let add = &[
        (vec![0x11, 0x00], vec![0x0a]),
        (vec![0x12, 0x10], vec![0x0b]),
        (vec![0x12, 0x20], vec![0x0b]),
        (vec![0x23, 0x00], vec![0x0b]),
        (vec![0x33, 0x00], vec![0x0c]),
    ];

    let remove = &[vec![0x11, 0x00]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn remove_branch_leave_1_children_ext_below_branch_above() {
    let add = &[
        (vec![0x11, 0x00, 0x00], vec![0x0a]),
        (vec![0x12, 0x10, 0x20], vec![0x0b]),
        (vec![0x12, 0x10, 0x30], vec![0x0b]),
        (vec![0x23, 0x00, 0x00], vec![0x0b]),
        (vec![0x33, 0x00, 0x00], vec![0x0c]),
    ];

    let remove = &[vec![0x11, 0x00, 0x00]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn remove_branch_leave_1_children_leaf_below_ext_above() {
    let add = &[(vec![0x11], vec![0x0a]), (vec![0x12], vec![0x0b])];

    let remove = &[vec![0x11]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn remove_branch_leave_1_children_branch_below_ext_above() {
    let add = &[
        (vec![0x11, 0x00], vec![0x0a]),
        (vec![0x12, 0x10], vec![0x0b]),
        (vec![0x12, 0x20], vec![0x0b]),
    ];

    let remove = &[vec![0x11, 0x00]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn remove_branch_leave_1_children_branch_below_null_above() {
    let add = &[
        (vec![0x10], vec![0xa]),
        (vec![0x23], vec![0xb]),
        (vec![0x24], vec![0xc]),
    ];

    let remove = &[vec![0x10]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn remove_branch_leave_1_children_ext_below_null_above() {
    let add = &[
        (vec![0x10, 0x00], vec![0xa]),
        (vec![0x23, 0x01], vec![0xb]),
        (vec![0x23, 0x02], vec![0xb]),
    ];

    let remove = &[vec![0x10, 0x00]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn remove_branch_leave_1_children_leaf_below_null_above() {
    let add = &[(vec![0x10, 0x00], vec![0xa]), (vec![0x23, 0x01], vec![0xb])];

    let remove = &[vec![0x10, 0x00]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn remove_branch_leave_1_children_ext_below_ext_above() {
    let add = &[
        (vec![0x11, 0x00], vec![0x0a]),
        (vec![0x12, 0x11], vec![0x0b]),
        (vec![0x12, 0x12], vec![0x0b]),
    ];

    let remove = &[vec![0x11, 0x00]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn failing_proptest_1() {
    let add = &[
        (vec![0xea, 0xbc, 0x01], vec![0x0a]),
        (vec![0xea, 0xbc, 0x10], vec![0x0b]),
    ];

    let remove = &[vec![0xea, 0xbc, 0x10]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn check_correct_gather_for_orphan_of_and_orphan() {
    // here we have 2 branch nodes with 2 children and both of them might have an orphan
    // the orphan of the branch 1 is branch 2 so its added to the result using different code path
    let add = &[
        (vec![0x00, 0x00, 0x00], vec![0x0a]),
        (vec![0x09, 0x00, 0x00], vec![0x0b]),
        (vec![0x09, 0x10, 0x00], vec![0x0c]),
    ];

    let remove = &[vec![0x00, 0x00, 0x00], vec![0x09, 0x00, 0x00]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn known_failure_case_0() {
    let input = StoredFailureCase::load("./test_data/failure_case_0.json.gz");

    let mut prev_value = None;
    for i in 0..10 {
        let mut input = input.clone();

        {
            let mut rng = rand::rngs::SmallRng::seed_from_u64(i);
            let mut key_values: Vec<_> = input
                .updated_keys
                .into_iter()
                .zip(input.updated_values)
                .collect();
            key_values.shuffle(&mut rng);
            (input.updated_keys, input.updated_values) = key_values.into_iter().unzip();
        }

        // random shuffle input

        for (key, value) in input.updated_keys.into_iter().zip(input.updated_values) {
            input.trie.insert(key, value).expect("must insert");
        }
        for key in input.deleted_keys {
            input.trie.delete(key).expect("must delete");
        }
        let hash = input.trie.root_hash().expect("must hash");
        if prev_value.is_none() {
            prev_value = Some(hash);
        } else {
            assert_eq!(prev_value, Some(hash), "seed:{i}");
        }
    }
}

proptest! {
    #[test]
    fn proptest_random_insert_any_values(key_values in any::<Vec<([u8; 3], Vec<u8>)>>()) {
        let data: Vec<_> = key_values.into_iter().map(|(k, v)| (k.to_vec(), v)).collect();
        compare_impls(&data);
    }


    #[test]
    fn proptest_random_insert_big_values(key_values in any::<Vec<([u8; 3], [u8; 64])>>()) {
        let data: Vec<_> = key_values.into_iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
        compare_impls(&data);
    }

    #[test]
    fn proptest_random_insert_small_values(key_values in any::<Vec<([u8; 3], [u8; 3])>>()) {
        let data: Vec<_> = key_values.into_iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
        compare_impls(&data);
    }

    #[test]
    fn proptest_random_insert_big_keys(key_values in any::<Vec<([u8; 32], Vec<u8>)>>()) {
        let data: Vec<_> = key_values.into_iter().map(|(k, v)| (k.to_vec(), v)).collect();
        compare_impls(&data);
    }


    #[test]
    fn proptest_random_insert_remove_any_values(key_values in any::<Vec<(([u8; 3], bool), Vec<u8>)>>()) {
        let mut keys_to_remove_set = HashSet::default();
        let mut keys_to_remove = Vec::new();
        let data: Vec<_> = key_values.into_iter().map(|((k, remove), v)| {
            if remove && !keys_to_remove_set.contains(&k) {
                keys_to_remove_set.insert(k);
                keys_to_remove.push(k.to_vec());
            }
            (k.to_vec(), v)
        }).collect();
        compare_with_removals(&data, &keys_to_remove).unwrap()
    }
}
