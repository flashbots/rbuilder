//! This library is useful when you need to calculate Ethereum root hash many times on top of the same parent block using reth database.
//!
//! To use this, for each parent block:
//! * create `SparseTrieSharedCache`
//! * call `calculate_root_hash_with_sparse_trie` with the given cache, reth db view and execution outcome.

#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::len_without_is_empty)]
#![allow(clippy::needless_range_loop)]

pub mod reth_sparse_trie;
pub mod sparse_mpt;
#[cfg(any(test, feature = "benchmark-utils"))]
pub mod test_utils;
pub mod utils;

pub use reth_sparse_trie::{
    calculate_root_hash_with_sparse_trie, prefetch_tries_for_accounts, ChangedAccountData,
    RootHashThreadPool, SparseTrieSharedCache,
};
