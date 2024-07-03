use ahash::HashMap;
use alloy_primitives::{Address, B256};
use reth::providers::{ProviderFactory, StateProviderBox};
use reth_db::database::Database;
use reth_interfaces::provider::ProviderResult;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct NonceCache<DB> {
    provider_factory: ProviderFactory<DB>,
    // we have to use Arc<Mutex here because Rc are not Send (so can't be used in futures)
    // and borrows don't work when nonce cache is a field in a struct
    cache: Arc<Mutex<HashMap<Address, u64>>>,
    block: B256,
}

impl<DB: Database> NonceCache<DB> {
    pub fn new(provider_factory: ProviderFactory<DB>, block: B256) -> Self {
        Self {
            provider_factory,
            cache: Arc::new(Mutex::new(HashMap::default())),
            block,
        }
    }

    pub fn get_ref(&self) -> ProviderResult<NonceCacheRef> {
        let state = self.provider_factory.history_by_block_hash(self.block)?;
        Ok(NonceCacheRef {
            state,
            cache: Arc::clone(&self.cache),
        })
    }
}

pub struct NonceCacheRef {
    state: StateProviderBox,
    cache: Arc<Mutex<HashMap<Address, u64>>>,
}

impl NonceCacheRef {
    pub fn nonce(&self, address: Address) -> ProviderResult<u64> {
        let mut cache = self.cache.lock().unwrap();
        if let Some(nonce) = cache.get(&address) {
            return Ok(*nonce);
        }
        let nonce = self.state.account_nonce(address)?.unwrap_or_default();
        cache.insert(address, nonce);
        Ok(nonce)
    }
}
