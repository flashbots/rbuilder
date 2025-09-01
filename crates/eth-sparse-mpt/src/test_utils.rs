use alloy_primitives::{keccak256, Bytes, B256};
use flate2::read::GzDecoder;
use rustc_hash::FxHasher;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{fs::File, io::Read, path::Path};

use crate::v1::{
    reth_sparse_trie::{change_set::ETHTrieChangeSet, trie_fetcher::MultiProof},
    sparse_mpt::DiffTrie,
};

pub fn deserialize_from_json_gzip<T: DeserializeOwned>(path: impl AsRef<Path>) -> eyre::Result<T> {
    let file = File::open(path)?;
    let mut gz = GzDecoder::new(file);
    let mut content = String::new();
    gz.read_to_string(&mut content)?;
    Ok(serde_json::from_str(&content)?)
}

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

pub fn reference_trie_hash_vec(data: &[(Vec<u8>, Vec<u8>)]) -> B256 {
    triehash::trie_root::<KeccakHasher, _, _, _>(data.to_vec())
}

pub fn get_test_multiproofs() -> Vec<MultiProof> {
    let files = [
        "./test_data/multiproof_0.json.gz",
        "./test_data/multiproof_1.json.gz",
    ];
    let mut result = Vec::new();
    for file in files {
        result.push(deserialize_from_json_gzip(file).expect("parsing multiproof"));
    }
    result
}

pub fn get_test_change_set() -> ETHTrieChangeSet {
    deserialize_from_json_gzip("./test_data/changeset.json.gz").expect("changeset")
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
        deserialize_from_json_gzip(path).expect("stored failure case")
    }
}
