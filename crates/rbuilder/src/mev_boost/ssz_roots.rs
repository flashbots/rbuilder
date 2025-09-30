//! SSZ utilities.

use alloy_primitives::{Address, Bytes, B256};
use sha2::{Digest, Sha256};
use ssz_types::{FixedVector, VariableList};
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

/// Generate SSZ proof for target transaction.
pub fn generate_transaction_proof_ssz(transactions: &[Bytes], target: usize) -> Vec<B256> {
    // Step 1: Compute all leaf hashes
    let mut leaves: Vec<B256> = transactions.iter().map(ssz_leaf_root).collect();

    // Step 2: Pad to the limit of the VariableList (2^20 for transactions)
    // SSZ always pads to the maximum possible size defined by the type
    let max_chunk_count = 1 << TREE_DEPTH; // 2^20 = 1,048,576

    // Resize to max_chunk_count with zero hashes
    leaves.resize(max_chunk_count, B256::ZERO);

    // Step 3: Build the merkle tree bottom-up and collect the proof
    let mut branch = Vec::new();
    let mut current_level = leaves;
    let mut current_index = target;

    // Build the complete tree to depth TREE_DEPTH (20 levels)
    for _level in 0..TREE_DEPTH {
        // Get the sibling at this level
        let sibling_index = current_index ^ 1;
        branch.push(current_level[sibling_index]);

        // Build next level up
        let mut next_level = Vec::new();
        for i in (0..current_level.len()).step_by(2) {
            let left = current_level[i];
            let right = current_level[i + 1];
            next_level.push(sha_pair(&left, &right));
        }

        current_level = next_level;
        current_index /= 2;

        // Stop when we reach the root
        if current_level.len() == 1 {
            break;
        }
    }

    branch
}

#[inline]
fn ssz_leaf_root(data: &Bytes) -> B256 {
    B256::from_slice(&BinaryTransaction::from(data.to_vec()).tree_hash_root()[..])
}

#[inline]
fn sha_pair(a: &B256, b: &B256) -> B256 {
    let mut h = Sha256::new();
    h.update(a);
    h.update(b);
    B256::from_slice(&h.finalize())
}
