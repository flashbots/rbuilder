use std::{collections::HashMap, env};

use super::*;
use crate::{test_utils::reference_trie_hash_vec, utils::HashSet};
use proptest::prelude::*;

fn compare_impls(data: &[(Vec<u8>, Vec<u8>)]) {
    let mut trie = Trie::new_empty();
    let empty_proof_store = ProofStore::default();
    for (key, value) in data {
        trie.insert(key, value).unwrap();
    }
    let got_hash = trie.root_hash(false, &empty_proof_store).unwrap();

    if std::env::var("ETH_SPARSE_MPT_TEST_PRINT").is_ok() {
        trie.debug_print_node(0);
    }
    let expected_hash = reference_trie_hash_vec(data);
    assert_eq!(expected_hash, got_hash);

    for (key, expected_value) in calculate_final_trie_values(data, &[]) {
        let ProofWithValue { proof, value } = trie
            .get_proof(&key, &empty_proof_store)
            .expect("failed to get proof");
        assert_eq!(expected_value, value, "proof value mismatch");
        let proof_hash = verify_proof(&key, proof);
        assert_eq!(got_hash, proof_hash);
    }
}

fn compare_with_removals(data: &[(Vec<u8>, Vec<u8>)], remove: &[Vec<u8>]) -> eyre::Result<()> {
    let empty_proof_store = ProofStore::default();
    let mut trie = Trie::new_empty();
    for (key, value) in data {
        trie.insert(key, value)?;
    }

    if env::var("ETH_SPARSE_MPT_TEST_PRINT").is_ok() {
        println!("Trie before deletes");
        trie.debug_print_node(0);
        println!();
    }
    for key in remove {
        trie.delete(key)?;
    }
    let got_hash = trie.root_hash(false, &empty_proof_store).unwrap();
    if env::var("ETH_SPARSE_MPT_TEST_PRINT").is_ok() {
        println!("Trie after deletes");
        trie.debug_print_node(0);
        println!();
    }

    let filtered_data: Vec<_> = data
        .iter()
        .filter(|(key, _)| !remove.contains(key))
        .cloned()
        .collect();

    if env::var("ETH_SPARSE_MPT_TEST_PRINT").is_ok() {
        // for reference trie without any removals
        println!("Trie from filtered data");
        let mut trie = Trie::new_empty();
        for (key, value) in &filtered_data {
            trie.insert(key, value).unwrap();
        }
        trie.root_hash(false, &empty_proof_store).unwrap();
        trie.debug_print_node(0);
        println!();
    }
    let expected_hash = reference_trie_hash_vec(&filtered_data);
    assert_eq!(expected_hash, got_hash);

    for (key, expected_value) in calculate_final_trie_values(data, remove) {
        let ProofWithValue { proof, value } = trie
            .get_proof(&key, &empty_proof_store)
            .expect("failed to get proof");
        assert_eq!(expected_value, value, "proof value mismatch");
        let proof_hash = verify_proof(&key, proof);
        assert_eq!(got_hash, proof_hash);
    }

    Ok(())
}

// resolves multiple inserts and removals
fn calculate_final_trie_values(
    data: &[(Vec<u8>, Vec<u8>)],
    remove: &[Vec<u8>],
) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    let mut result: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::default();
    for (key, value) in data {
        result.insert(key.clone(), Some(value.clone()));
    }
    for key in remove {
        result.insert(key.clone(), None);
    }
    let mut result: Vec<_> = result.into_iter().collect();
    result.sort_by_key(|(k, _)| k.clone());
    result
}

fn verify_proof(key: &[u8], proof: Vec<(Nibbles, Vec<u8>)>) -> B256 {
    let nibble_key = Nibbles::unpack(key);
    let proof_store = ProofStore::default();
    proof_store
        .add_proof(nibble_key.clone(), proof)
        .expect("failed to add proof to proof store");
    let mut trie = Trie::default();
    let found = trie
        .try_add_proof_from_proof_store(&nibble_key, &proof_store)
        .expect("failed to add proof to the trie");
    assert!(found, "proof was not found in proof store");
    trie.root_hash(false, &proof_store)
        .expect("failed to calc root hash from proof")
}

#[test]
fn do_empty_trie() {
    compare_impls(&[]);
}

#[test]
fn do_one_element_trie() {
    let data = [(vec![1, 1], vec![0xa, 0xa])];
    compare_impls(&data);
}

#[test]
fn do_update_leaf_node() {
    let data = &[(vec![1], vec![2]), (vec![1], vec![3])];
    compare_impls(data);
}

#[test]
fn do_insert_into_leaf_node_no_extension() {
    let data = &[(vec![0x11], vec![0x0a]), (vec![0x22], vec![0x0b])];
    compare_impls(data);

    let data = &[(vec![0x22], vec![0x0b]), (vec![0x11], vec![0x0a])];
    compare_impls(data);
}
#[test]
fn do_insert_into_leaf_node_with_extension() {
    let data = &[
        (vec![0x33, 0x22], vec![0x0a]),
        (vec![0x33, 0x11], vec![0x0b]),
    ];
    compare_impls(data);
}

#[test]
fn do_insert_into_extension_node_no_extension_above() {
    let data = &[
        (vec![0x33, 0x22], vec![0x0a]),
        (vec![0x33, 0x11], vec![0x0b]),
        (vec![0x44, 0x33], vec![0x0c]),
    ];
    compare_impls(data);
}

#[test]
fn do_insert_into_extension_node_with_extension_above() {
    let data = &[
        (vec![0x33, 0x33, 0x22], vec![0x0a]),
        (vec![0x33, 0x33, 0x11], vec![0x0b]),
        (vec![0x33, 0x44, 0x33], vec![0x0c]),
    ];
    compare_impls(data);
}

#[test]
fn do_insert_into_extension_node_collapse_extension() {
    let data = &[
        (vec![0x33, 0x22, 0x44], vec![0x0a]),
        (vec![0x33, 0x11, 0x44], vec![0x0b]),
        (vec![0x34, 0x33, 0x44], vec![0x0c]),
    ];
    compare_impls(data);
}

#[test]
fn do_insert_into_extension_node_collapse_extension_no_ext_above() {
    let data = &[
        (vec![0x31, 0x11], vec![0x0a]),
        (vec![0x32, 0x22], vec![0x0b]),
        (vec![0x11, 0x33], vec![0x0c]),
    ];
    compare_impls(data);
}

#[test]
fn do_insert_into_branch_empty_child() {
    let data = &[
        (vec![0x11], vec![0x0a]),
        (vec![0x22], vec![0x0b]),
        (vec![0x33], vec![0x0c]),
    ];
    compare_impls(data);
}

#[test]
fn do_insert_into_branch_leaf_child() {
    let data = &[
        (vec![0x11], vec![0x0a]),
        (vec![0x22], vec![0x0b]),
        (vec![0x33], vec![0x0c]),
        (vec![0x33], vec![0x0d]),
    ];
    compare_impls(data);
}

#[test]
fn do_remove_empty_trie_err() {
    let add = &[];

    let remove = &[vec![0x12]];

    let _ = compare_with_removals(add, remove).unwrap_err();
}

#[test]
fn do_remove_leaf() {
    let add = &[(vec![0x11], vec![0x0a])];

    let remove = &[vec![0x11]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn do_remove_leaf_key_error() {
    let add = &[(vec![0x11], vec![0x0a])];

    let remove = &[vec![0x12]];

    let _ = compare_with_removals(add, remove).unwrap_err();
}

#[test]
fn do_remove_extension_node_error() {
    let add = &[(vec![0x11, 0x1], vec![0x0a]), (vec![0x11, 0x2], vec![0x0b])];

    let remove = &[vec![0x12]];

    let _ = compare_with_removals(add, remove).unwrap_err();
}

#[test]
fn do_remove_branch_err() {
    let add = &[
        (vec![0x01, 0x10], vec![0x0a]),
        (vec![0x01, 0x20], vec![0x0b]),
        (vec![0x01, 0x30], vec![0x0c]),
    ];

    let remove = &[vec![0x01]];

    let _ = compare_with_removals(add, remove).unwrap_err();
}

#[test]
fn do_remove_branch_leave_2_children() {
    let add = &[
        (vec![0x01], vec![0x0a]),
        (vec![0x02], vec![0x0b]),
        (vec![0x03], vec![0x0c]),
    ];

    let remove = &[vec![0x01]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn do_remove_branch_leave_1_children_leaf_below_branch_above() {
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
fn do_remove_branch_leave_1_children_branch_below_branch_above() {
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
fn do_remove_branch_leave_1_children_ext_below_branch_above() {
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
fn do_remove_branch_leave_1_children_leaf_below_ext_above() {
    let add = &[(vec![0x11], vec![0x0a]), (vec![0x12], vec![0x0b])];

    let remove = &[vec![0x11]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn do_remove_branch_leave_1_children_branch_below_ext_above() {
    let add = &[
        (vec![0x11, 0x00], vec![0x0a]),
        (vec![0x12, 0x10], vec![0x0b]),
        (vec![0x12, 0x20], vec![0x0b]),
    ];

    let remove = &[vec![0x11, 0x00]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn do_remove_branch_leave_1_children_branch_below_null_above() {
    let add = &[
        (vec![0x10], vec![0xa]),
        (vec![0x23], vec![0xb]),
        (vec![0x24], vec![0xc]),
    ];

    let remove = &[vec![0x10]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn do_remove_branch_leave_1_children_ext_below_null_above() {
    let add = &[
        (vec![0x10, 0x00], vec![0xa]),
        (vec![0x23, 0x01], vec![0xb]),
        (vec![0x23, 0x02], vec![0xb]),
    ];

    let remove = &[vec![0x10, 0x00]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn do_remove_branch_leave_1_children_leaf_below_null_above() {
    let add = &[(vec![0x10, 0x00], vec![0xa]), (vec![0x23, 0x01], vec![0xb])];

    let remove = &[vec![0x10, 0x00]];

    compare_with_removals(add, remove).unwrap();
}

#[test]
fn do_remove_branch_leave_1_children_ext_below_ext_above() {
    let add = &[
        (vec![0x11, 0x00], vec![0x0a]),
        (vec![0x12, 0x11], vec![0x0b]),
        (vec![0x12, 0x12], vec![0x0b]),
    ];

    let remove = &[vec![0x11, 0x00]];

    compare_with_removals(add, remove).unwrap();
}

fn compare_insert_key_hashed_map(
    fill_values: &[(Vec<u8>, Vec<u8>)],
    last_value: &(Vec<u8>, Vec<u8>),
) {
    let mut trie = Trie::new_empty();
    let proof_store = ProofStore::default();

    for (key, value) in fill_values {
        trie.insert(key, value).expect("insert fill");
    }
    trie.root_hash(true, &proof_store).expect("hash fill");

    trie.insert(&last_value.0, &last_value.1)
        .expect("insert last");
    let hash1 = trie.root_hash(true, &proof_store).expect("hash last");

    let mut trie = Trie::new_empty();

    for (key, value) in fill_values {
        trie.insert(key, value).expect("insert fill 2");
    }
    trie.insert(&last_value.0, &last_value.1)
        .expect("insert last 2");
    let hash2 = trie.root_hash(true, &proof_store).expect("hash 2");

    assert_eq!(hash1, hash2);
}

fn compare_insert_key_reinsert(
    // key, value, new value (or delete)
    input: &[(Vec<u8>, Vec<u8>, Option<Vec<u8>>)],
) {
    let proof_store = ProofStore::default();

    let mut trie = Trie::new_empty();
    for (key, value, _) in input {
        trie.insert(key, value).expect("insert fill");
    }
    trie.root_hash(true, &proof_store).expect("hash fill");
    for (key, _, new_value) in input {
        match new_value.as_ref() {
            Some(value) => {
                trie.insert(key, value).expect("second pass insert");
            }
            None => {
                trie.delete(key).expect("second pass delete");
            }
        }
    }
    let hash1 = trie
        .root_hash(true, &proof_store)
        .expect("second pass hash");

    let mut ref_trie = Trie::new_empty();
    for (key, _, new_value) in input {
        if let Some(value) = new_value.as_ref() {
            ref_trie.insert(key, value).expect("ref pass insert");
        }
    }
    let hash2 = ref_trie.root_hash(true, &proof_store).expect("ref hash");

    assert_eq!(hash1, hash2);
}

proptest! {
    #[test]
    fn proptest_random_insert_small_values(key_values in any::<Vec<([u8; 3], [u8; 3])>>()) {
        let data: Vec<_> = key_values.into_iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
        compare_impls(&data);
    }

    #[test]
    fn proptest_random_insert_reinsert(key_values in any::<Vec<([u8; 3], [u8; 3], Option<[u8; 3]>)>>()) {
        let data: Vec<_> = key_values.into_iter().map(|(k, v1, v2)| (k.to_vec(), v1.to_vec(), v2.map(|v| v.to_vec()))).collect();
        compare_insert_key_reinsert(&data);
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
        compare_with_removals(&data, &keys_to_remove).unwrap();
    }

    #[test]
    fn proptest_insert_key_hashed_map(key_values in any::<Vec<([u8; 3], Vec<u8>)>>(), last_value in any::<([u8; 3], Vec<u8>)>()) {
        let data: Vec<_> = key_values.into_iter().map(|(k, v)| (k.to_vec(), v)).collect();
    let last_value = (last_value.0.to_vec(), last_value.1);
    compare_insert_key_hashed_map(&data, &last_value);
    }
}
