use alloy_primitives::Address;
use dashmap::DashMap;
use derivative::Derivative;
use reth_errors::ProviderResult;
use reth_provider::StateProvider;
use std::sync::Arc;

/// Struct to get nonces for Addresses, caching the results.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct NonceCache {
    #[derivative(Debug = "ignore")]
    state: Arc<dyn StateProvider>,
    cache: Arc<DashMap<Address, u64>>,
}

impl NonceCache {
    pub fn new(state: Arc<dyn StateProvider>) -> Self {
        Self {
            state,
            cache: Arc::new(DashMap::default()),
        }
    }

    pub fn nonce(&self, address: Address) -> ProviderResult<u64> {
        if let Some(nonce) = self.cache.get(&address) {
            return Ok(*nonce);
        }

        let nonce = self.state.account_nonce(&address)?.unwrap_or_default();
        self.cache.insert(address, nonce);
        Ok(nonce)
    }
}
