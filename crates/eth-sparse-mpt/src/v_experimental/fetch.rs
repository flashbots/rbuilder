use std::sync::Arc;

use crate::{
    utils::{convert_nibbles_to_reth_nybbles, convert_reth_nybbles_to_nibbles, HashMap},
    SparseTrieError,
};
use alloy_primitives::map::B256Set;
use parking_lot::Mutex;
use rayon::prelude::*;

use alloy_primitives::B256;
use nybbles::Nibbles;
use reth_provider::{
    providers::ConsistentDbView, BlockHashReader, BlockNumReader, BlockReader, DBProvider,
    DatabaseProviderFactory,
};
use reth_trie::{
    proof::{Proof, StorageProof},
    MultiProofTargets,
};
use reth_trie_db::{DatabaseHashedCursorFactory, DatabaseTrieCursorFactory};

use super::SharedCacheV2;

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
        entry.0.insert(pad_path(node.clone()));
        entry.1.push(node);
    }

    pub fn add_missing_account_node(&mut self, node: Nibbles) {
        self.account_proof_targets.push(pad_path(node.clone()));
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

        let last_block_hash = shared_cache.last_block_hash;
        std::mem::take(&mut self.storage_proof_targets)
            .into_par_iter()
            .map(
                |(hashed_address, (targets, requested_proofs))| -> Result<(), SparseTrieError> {
                    let provider = consistent_db_view
                        .provider_ro()
                        .map_err(SparseTrieError::other)?;
                    if !last_block_hash.is_zero() {
                        let block_number = provider
                            .last_block_number()
                            .map_err(SparseTrieError::other)?;
                        let block_hash = provider
                            .block_hash(block_number)
                            .map_err(SparseTrieError::other)?;
                        if block_hash != Some(shared_cache.last_block_hash) {
                            return Err(SparseTrieError::WrongDatabaseTrieError);
                        }
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
                        let proof_for_node = storge_multiproof.subtree.matching_nodes_sorted(
                            &convert_nibbles_to_reth_nybbles(requested_proof.clone()),
                        );
                        let reth_proof_for_node = proof_for_node
                            .into_iter()
                            .map(|(k, v)| (convert_reth_nybbles_to_nibbles(k), v))
                            .collect();
                        let proof_store =
                            shared_cache.account_proof_store_hashed_address(&hashed_address);
                        proof_store
                            .add_proof(requested_proof, reth_proof_for_node)
                            .map_err(SparseTrieError::other)?;
                    }
                    Ok(())
                },
            )
            .collect::<Result<(), _>>()?;

        let provider = consistent_db_view
            .provider_ro()
            .map_err(SparseTrieError::other)?;
        if !last_block_hash.is_zero() {
            let block_number = provider
                .last_block_number()
                .map_err(SparseTrieError::other)?;
            let block_hash = provider
                .block_hash(block_number)
                .map_err(SparseTrieError::other)?;
            if block_hash != Some(shared_cache.last_block_hash) {
                return Err(SparseTrieError::WrongDatabaseTrieError);
            }
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
                .matching_nodes_sorted(&convert_nibbles_to_reth_nybbles(requested_node.clone()));

            let reth_proof_for_node = proof_for_node
                .into_iter()
                .map(|(k, v)| (convert_reth_nybbles_to_nibbles(k), v))
                .collect();
            shared_cache
                .account_trie
                .add_proof(requested_node, reth_proof_for_node)
                .map_err(SparseTrieError::other)?;
        }
        let fetched_nodes = *fetched_nodes.lock();
        Ok(fetched_nodes)
    }
}

fn pad_path(mut path: Nibbles) -> B256 {
    path.as_mut_vec_unchecked().resize(64, 0);
    let mut res = B256::default();
    path.pack_to(res.as_mut_slice());
    res
}
