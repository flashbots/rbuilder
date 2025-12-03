use super::change_set::ETHTrieChangeSet;
use crate::{
    utils::HashMap,
    v1::sparse_mpt::{DeletionError, DiffTrie, ErrSparseNodeNotFound},
};
use alloy_primitives::{Bytes, B256};
use alloy_rlp::Encodable;
use rayon::prelude::*;
use reth_trie::TrieAccount;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, Seq};

#[serde_as]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EthSparseTries {
    pub account_trie: DiffTrie,
    #[serde_as(as = "Seq<(_, _)>")]
    pub storage_tries: HashMap<Bytes, DiffTrie>,
}

#[derive(Debug, thiserror::Error)]
pub enum RootHashError {
    #[error("Storage trie not found, account: {0:?}")]
    StorageTrieNotFound(Bytes),
    #[error(
        "Error while updating in storage trie, account: {account:?}, key: {key:?}, err: {err:?}"
    )]
    UpdatingStorageTrie {
        account: Bytes,
        key: Bytes,
        err: ErrSparseNodeNotFound,
    },
    #[error(
        "Error while deleting from storage trie, account: {account:?}, key: {key:?}, err: {err:?}"
    )]
    DeletingStorageTrie {
        account: Bytes,
        key: Bytes,
        err: DeletionError,
    },
    #[error("Error while hashing storage trie, account: {account:?}, err: {err:?}")]
    HashingStorageTrie {
        account: Bytes,
        err: ErrSparseNodeNotFound,
    },
    #[error("Error while updating in accounts trie, account: {account:?}, err: {err:?}")]
    UpdatingAccountsTrie {
        account: Bytes,
        err: ErrSparseNodeNotFound,
    },
    #[error("Error while deleting from accounts trie, account: {account:?}, err: {err:?}")]
    DeletingAccountsTrie { account: Bytes, err: DeletionError },
    #[error("Error while hashing account trie, err: {err:?}")]
    HashingAccountsTrie { err: ErrSparseNodeNotFound },
}

impl EthSparseTries {
    #[allow(clippy::result_large_err)]
    pub fn calculate_root_hash(
        &mut self,
        changes: ETHTrieChangeSet,
        parallel_storage: bool,
        parallel_main_trie: bool,
    ) -> Result<B256, RootHashError> {
        let mut account_hashes = if parallel_storage {
            self.calculate_account_hashes_parallel(&changes)?
        } else {
            self.calculate_account_hashes_seq(&changes)?
        };

        let mut encoded_account = Vec::new();
        for (account, updated_info) in changes
            .account_trie_updates
            .into_iter()
            .zip(changes.account_trie_updates_info)
        {
            let hash = account_hashes
                .remove(&account)
                .expect("account hash not found");
            let trie_account: TrieAccount = TrieAccount {
                nonce: updated_info.nonce,
                balance: updated_info.balance,
                storage_root: hash,
                code_hash: updated_info.code_hash,
            };
            encoded_account.clear();
            trie_account.encode(&mut encoded_account);

            self.account_trie
                .insert(account.clone(), Bytes::copy_from_slice(&encoded_account))
                .map_err(|err| RootHashError::UpdatingAccountsTrie {
                    account: account.clone(),
                    err,
                })?;
        }

        for account in &changes.account_trie_deletes {
            self.account_trie.delete(account.clone()).map_err(|err| {
                RootHashError::DeletingAccountsTrie {
                    account: account.clone(),
                    err,
                }
            })?;
        }
        if parallel_main_trie {
            self.account_trie
                .root_hash_parallel()
                .map_err(|err| RootHashError::HashingAccountsTrie { err })
        } else {
            self.account_trie
                .root_hash()
                .map_err(|err| RootHashError::HashingAccountsTrie { err })
        }
    }

    #[allow(clippy::result_large_err)]
    fn calculate_account_hashes_seq(
        &mut self,
        changes: &ETHTrieChangeSet,
    ) -> Result<HashMap<Bytes, B256>, RootHashError> {
        let mut account_hashes = HashMap::default();

        for (idx, account) in changes.account_trie_updates.iter().enumerate() {
            let updated_keys = &changes.storage_trie_updated_keys[idx];
            let updated_values = &changes.storage_trie_updated_values[idx];
            let deleted_keys = &changes.storage_trie_deleted_keys[idx];

            let storage_trie = self
                .storage_tries
                .get_mut(account)
                .ok_or_else(|| RootHashError::StorageTrieNotFound(account.clone()))?;

            let storage_hash = hash_storage_trie(
                storage_trie,
                account,
                updated_keys,
                updated_values,
                deleted_keys,
            )?;
            account_hashes.insert(account.clone(), storage_hash);
        }
        Ok(account_hashes)
    }

    #[allow(clippy::result_large_err)]
    fn calculate_account_hashes_parallel(
        &mut self,
        changes: &ETHTrieChangeSet,
    ) -> Result<HashMap<Bytes, B256>, RootHashError> {
        let mut input = Vec::with_capacity(changes.account_trie_updates.len());

        for (idx, account) in changes.account_trie_updates.iter().enumerate() {
            let updated_keys = &changes.storage_trie_updated_keys[idx];
            let updated_values = &changes.storage_trie_updated_values[idx];
            let deleted_keys = &changes.storage_trie_deleted_keys[idx];

            let storage_trie = self
                .storage_tries
                .remove(account)
                .ok_or_else(|| RootHashError::StorageTrieNotFound(account.clone()))?;
            input.push((
                account,
                storage_trie,
                updated_keys,
                updated_values,
                deleted_keys,
            ));
        }

        let account_hashes = input.into_par_iter().map(|(account, mut storage_trie, updated_keys, updated_values, deleted_keys)| -> Result<_,_> {
	    let storage_hash = hash_storage_trie(&mut storage_trie, account, updated_keys, updated_values, deleted_keys)?;
   	    Ok((account.clone(), storage_hash))
	}).collect::<Result<HashMap<_, _>, _>>()?;
        Ok(account_hashes)
    }
}

#[allow(clippy::result_large_err)]
fn hash_storage_trie(
    storage_trie: &mut DiffTrie,
    account: &Bytes,
    updated_keys: &[Bytes],
    updated_values: &[Bytes],
    deleted_keys: &[Bytes],
) -> Result<B256, RootHashError> {
    for (key, value) in updated_keys.iter().zip(updated_values) {
        storage_trie
            .insert(key.clone(), value.clone())
            .map_err(|err| RootHashError::UpdatingStorageTrie {
                account: account.clone(),
                key: key.clone(),
                err,
            })?;
    }
    for key in deleted_keys {
        storage_trie
            .delete(key.clone())
            .map_err(|err| RootHashError::DeletingStorageTrie {
                account: account.clone(),
                key: key.clone(),
                err,
            })?;
    }
    storage_trie
        .root_hash()
        .map_err(|err| RootHashError::HashingStorageTrie {
            account: account.clone(),
            err,
        })
}
