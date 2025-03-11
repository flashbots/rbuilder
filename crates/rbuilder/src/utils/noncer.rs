use crate::provider::StateProviderFactory;
use alloy_primitives::{Address, B256};
use dashmap::DashMap;
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
    // We use Arc<DashMap> here to allow concurrent access to cache
    cache: Arc<DashMap<Address, u64>>,
    block: B256,
}

impl<P> NonceCache<P>
where
    P: StateProviderFactory,
{
    pub fn new(provider: P, block: B256) -> Self {
        Self {
            provider,
            cache: Arc::new(DashMap::default()),
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
    cache: Arc<DashMap<Address, u64>>,
}

impl NonceCacheRef {
    pub fn nonce(&self, address: Address) -> ProviderResult<u64> {
        if let Some(nonce) = self.cache.get(&address) {
            return Ok(*nonce);
        }

        let nonce = self.state.account_nonce(&address)?.unwrap_or_default();
        self.cache.insert(address, nonce);
        Ok(nonce)
    }
}
