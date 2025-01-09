use crate::provider::StateProviderFactory;
use ahash::HashMap;
use alloy_primitives::{Address, B256};
use parking_lot::Mutex;
use reth::providers::StateProviderBox;
use reth_errors::ProviderResult;
use std::sync::Arc;

/// Struct to get nonces for Addresses, caching the results.
/// NonceCache contains the data (but doesn't allow you to query it) and NonceCacheRef is a reference that allows you to query it.
/// Usage:
/// - Create a NonceCache
/// - For every context where the nonce is needed call NonceCache::get_ref and call NonceCacheRef::nonce all the times you need.
///   Neither NonceCache or NonceCacheRef are clonable, the clone of shared info happens on get_ref where we clone the internal cache.
#[derive(Debug)]
pub struct NonceCache<P> {
    provider: P,
    // We have to use Arc<Mutex here because Rc are not Send (so can't be used in futures)
    // and borrows don't work when nonce cache is a field in a struct.
    cache: Arc<Mutex<HashMap<Address, u64>>>,
    block: B256,
}

impl<P> NonceCache<P>
where
    P: StateProviderFactory,
{
    pub fn new(provider: P, block: B256) -> Self {
        Self {
            provider,
            cache: Arc::new(Mutex::new(HashMap::default())),
            block,
        }
    }

    pub fn get_ref(&self) -> ProviderResult<NonceCacheRef> {
        let state = self.provider.history_by_block_hash(self.block)?;
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
        let mut cache = self.cache.lock();
        if let Some(nonce) = cache.get(&address) {
            return Ok(*nonce);
        }
        let nonce = self.state.account_nonce(address)?.unwrap_or_default();
        cache.insert(address, nonce);
        Ok(nonce)
    }
}
