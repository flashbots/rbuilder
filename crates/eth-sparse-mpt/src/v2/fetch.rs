use std::sync::Arc;

use crate::{utils::HashMap, SparseTrieError};
use alloy_primitives::map::B256Set;
use parking_lot::Mutex;
use rayon::prelude::*;

use alloy_primitives::B256;
use nybbles::Nibbles;
use reth_provider::{
    providers::ConsistentDbView, BlockReader, DBProvider, DatabaseProviderFactory,
};
use reth_trie::{
    proof::{Proof, StorageProof},
    MultiProofTargets, StateRoot,
};
use reth_trie_db::{DatabaseHashedCursorFactory, DatabaseStateRoot, DatabaseTrieCursorFactory};

use super::SharedCacheV2;

pub fn check_state_root_in_db(
    provider: &impl DBProvider,
    expected_state_root: B256,
) -> Result<(), SparseTrieError> {
    let db_state_root = StateRoot::from_tx(provider.tx_ref())
        .root()
        .map_err(SparseTrieError::other)?;
    if db_state_root == expected_state_root {
        Ok(())
    } else {
        Err(SparseTrieError::WrongDatabaseTrieError)
    }
}

#[derive(Debug, Default)]
pub struct MissingNodesFetcher {
    storage_proof_targets: HashMap<B256, (B256Set, Vec<Nibbles>)>,
    account_proof_targets: Vec<B256>,
    account_proof_requested_nodes: Vec<Nibbles>,
}

impl MissingNodesFetcher {
    pub fn is_empty(&self) -> bool {
        self.storage_proof_targets.is_empty() && self.account_proof_targets.is_empty()
    }

    pub fn add_missing_storage_node(&mut self, hashed_address: &B256, node: Nibbles) {
        let entry = self
            .storage_proof_targets
            .entry(*hashed_address)
            .or_default();
        entry.0.insert(pad_path(node));
        entry.1.push(node);
    }

    pub fn add_missing_account_node(&mut self, node: Nibbles) {
        self.account_proof_targets.push(pad_path(node));
        self.account_proof_requested_nodes.push(node);
    }

    // fetch currently accumulated nodes into shared cache
    pub fn fetch_nodes<Provider>(
        &mut self,
        shared_cache: &SharedCacheV2,
        consistent_db_view: &ConsistentDbView<Provider>,
    ) -> Result<usize, SparseTrieError>
    where
        Provider: DatabaseProviderFactory<Provider: BlockReader> + Send + Sync,
    {
        let fetched_nodes: Arc<Mutex<usize>> = Default::default();

        let parent_state_root = shared_cache.parent_state_root;
        std::mem::take(&mut self.storage_proof_targets)
            .into_par_iter()
            .map(
                |(hashed_address, (targets, requested_proofs))| -> Result<(), SparseTrieError> {
                    let provider = consistent_db_view
                        .provider_ro()
                        .map_err(SparseTrieError::other)?;
                    if !parent_state_root.is_zero() {
                        check_state_root_in_db(&provider, parent_state_root)?;
                    }

                    let proof = StorageProof::new_hashed(
                        DatabaseTrieCursorFactory::new(provider.tx_ref()),
                        DatabaseHashedCursorFactory::new(provider.tx_ref()),
                        hashed_address,
                    );
                    let storge_multiproof = proof
                        .storage_multiproof(targets)
                        .map_err(SparseTrieError::other)?;
                    *fetched_nodes.lock() += requested_proofs.len();
                    for requested_proof in requested_proofs {
                        let proof_for_node = storge_multiproof
                            .subtree
                            .matching_nodes_sorted(&requested_proof);
                        let proof_store =
                            shared_cache.account_proof_store_hashed_address(&hashed_address);
                        proof_store
                            .add_proof(requested_proof, proof_for_node)
                            .map_err(SparseTrieError::other)?;
                    }
                    Ok(())
                },
            )
            .collect::<Result<(), _>>()?;

        let provider = consistent_db_view
            .provider_ro()
            .map_err(SparseTrieError::other)?;
        if !parent_state_root.is_zero() {
            check_state_root_in_db(&provider, parent_state_root)?
        }

        let proof = Proof::new(
            DatabaseTrieCursorFactory::new(provider.tx_ref()),
            DatabaseHashedCursorFactory::new(provider.tx_ref()),
        );
        let targets = MultiProofTargets::accounts(std::mem::take(&mut self.account_proof_targets));
        let multiproof = proof.multiproof(targets).map_err(SparseTrieError::other)?;

        *fetched_nodes.lock() += self.account_proof_requested_nodes.len();
        for requested_node in self.account_proof_requested_nodes.drain(..) {
            let proof_for_node = multiproof
                .account_subtree
                .matching_nodes_sorted(&requested_node);

            shared_cache
                .account_trie
                .add_proof(requested_node, proof_for_node)
                .map_err(SparseTrieError::other)?;
        }
        let fetched_nodes = *fetched_nodes.lock();
        Ok(fetched_nodes)
    }
}

fn pad_path(path: Nibbles) -> B256 {
    // `pack_to` fills the first `byte_len` bytes; the remaining bytes stay zero,
    // which is equivalent to padding the path with zero nibbles up to 64.
    let mut res = B256::default();
    path.pack_to(res.as_mut_slice());
    res
}
