//! SSZ utilities.

use alloy_primitives::{Address, Bytes, B256};
use sha2::{Digest, Sha256};
use ssz_types::{FixedVector, VariableList};
use std::sync::LazyLock;
use tree_hash::TreeHash as _;

#[derive(tree_hash_derive::TreeHash)]
struct TreeHashAddress {
    inner: FixedVector<u8, typenum::U20>,
}

impl From<Address> for TreeHashAddress {
    fn from(address: Address) -> Self {
        Self {
            inner: FixedVector::from(address.to_vec()),
        }
    }
}

#[derive(tree_hash_derive::TreeHash)]
struct Withdrawal {
    pub index: u64,
    pub validator_index: u64,
    pub address: TreeHashAddress,
    pub amount: u64,
}

type MaxWithdrawalsPerPayload = typenum::U16;

/// Calculate SSZ root for withdrawals.
pub fn calculate_withdrawals_root_ssz(withdrawals: &[alloy_eips::eip4895::Withdrawal]) -> B256 {
    let withdrawals: VariableList<Withdrawal, MaxWithdrawalsPerPayload> = VariableList::from(
        withdrawals
            .iter()
            .map(|w| Withdrawal {
                index: w.index,
                validator_index: w.validator_index,
                address: TreeHashAddress::from(w.address),
                amount: w.amount,
            })
            .collect::<Vec<_>>(),
    );
    B256::from_slice(&withdrawals.tree_hash_root()[..])
}

type MaxBytesPerTransaction = typenum::U1073741824;
type MaxTransactionsPerPayload = typenum::U1048576;
type BinaryTransaction = VariableList<u8, MaxBytesPerTransaction>;

/// Calculate SSZ root for transactions.
pub fn calculate_transactions_root_ssz(transactions: &[Bytes]) -> B256 {
    let transactions: VariableList<BinaryTransaction, MaxTransactionsPerPayload> =
        VariableList::from(
            transactions
                .iter()
                .map(|bytes| BinaryTransaction::from(bytes.to_vec()))
                .collect::<Vec<_>>(),
        );
    B256::from_slice(&transactions.tree_hash_root()[..])
}

const TREE_DEPTH: usize = 20; // log₂(MAX_TRANSACTIONS_PER_PAYLOAD)

// Precompute HASHES[k] = hash of a full-zero subtree at level k.
static ZERO_SUBTREE: LazyLock<[B256; TREE_DEPTH + 1]> = LazyLock::new(|| {
    let mut hashes = [B256::ZERO; TREE_DEPTH + 1];
    for lvl in 0..TREE_DEPTH {
        hashes[lvl + 1] = sha_pair(&hashes[lvl], &hashes[lvl]);
    }
    hashes
});

#[derive(Debug)]
pub struct CompactSszTransactionTree(Vec<Vec<B256>>);

impl CompactSszTransactionTree {
    /// Build a compact Merkle tree over `n = txs.len()` leaves.
    /// Level 0 = leaves; Level k has len = ceil(prev_len/2).
    /// Padding beyond n uses structural zeros Z[k].
    pub fn from_leaves(mut leaves: Vec<B256>) -> Self {
        // Degenerate case: treat as single zero leaf so we still have a root
        if leaves.is_empty() {
            leaves.push(ZERO_SUBTREE[0]);
        }

        // Level 0: leaves
        let mut levels: Vec<Vec<B256>> = Vec::new();
        levels.push(leaves);

        // Upper levels
        for level in 0..TREE_DEPTH {
            let prev = &levels[level];
            if prev.len() == 1 {
                break; // reached root
            }
            let parents = prev.len().div_ceil(2);
            let mut next = Vec::with_capacity(parents);
            for i in 0..parents {
                // NOTE: left node should always be set
                let l = prev.get(2 * i).copied().unwrap_or(ZERO_SUBTREE[level]);
                let r = prev.get(2 * i + 1).copied().unwrap_or(ZERO_SUBTREE[level]);
                next.push(sha_pair(&l, &r));
            }
            levels.push(next);
        }

        Self(levels)
    }

    pub fn proof(&self, target: usize) -> Vec<B256> {
        let mut branch = Vec::with_capacity(TREE_DEPTH);
        for level in 0..TREE_DEPTH {
            if level >= self.0.len() || self.0[level].len() == 1 {
                // Either level wasn't built or compact root reached - structural zero sibling.
                branch.push(ZERO_SUBTREE[level]);
                continue;
            }

            let segment_index = target >> level;
            let sibling_index = segment_index ^ 1;
            let sibling = self.0[level]
                .get(sibling_index)
                .copied()
                .unwrap_or(ZERO_SUBTREE[level]); // structural zero if beyond built range
            branch.push(sibling);
        }

        branch
    }
}

/// Create the leaf root for transaction bytes.
#[inline]
pub fn tx_ssz_leaf_root(data: &[u8]) -> B256 {
    B256::from_slice(&BinaryTransaction::from(data.to_vec()).tree_hash_root()[..])
}

/// Compute a SHA-256 hash of the pair of 32 byte hashes.
#[inline]
pub fn sha_pair(a: &B256, b: &B256) -> B256 {
    let mut h = Sha256::new();
    h.update(a);
    h.update(b);
    B256::from_slice(&h.finalize())
}
