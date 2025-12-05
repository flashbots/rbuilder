use crate::telemetry::{inc_precompile_cache_hits, inc_precompile_cache_misses};
use ahash::HashMap;
use alloy_primitives::{Address, Bytes};
use derive_more::{Deref, DerefMut};
use lru::LruCache;
use parking_lot::Mutex;
use revm::{
    context::{Cfg, ContextTr},
    handler::PrecompileProvider,
    interpreter::{CallInputs, InterpreterResult},
    primitives::hardfork::SpecId,
};
use std::{num::NonZeroUsize, sync::Arc};

/// A precompile cache that stores precompile call results by precompile address.
#[derive(Deref, DerefMut, Default, Debug)]
pub struct PrecompileCache(HashMap<Address, PrecompileResultCache>);

/// Precompile result LRU cache  stored by `(spec id, input, gas limit)` key.
pub type PrecompileResultCache = LruCache<(SpecId, Bytes, u64), Result<InterpreterResult, String>>;

/// A custom precompile that contains the cache and precompile it wraps.
#[derive(Clone)]
pub struct WrappedPrecompile<P> {
    /// The precompile to wrap.
    precompile: P,
    /// The cache to use.
    cache: Arc<Mutex<PrecompileCache>>,
    /// The spec id to use.
    spec: SpecId,
}

impl<P> WrappedPrecompile<P> {
    /// Given a [`PrecompileProvider`] and cache for a specific precompiles, create a
    /// wrapper that can be used inside Evm.
    pub fn new(precompile: P, cache: Arc<Mutex<PrecompileCache>>) -> Self {
        WrappedPrecompile {
            precompile,
            cache: cache.clone(),
            spec: SpecId::default(),
        }
    }
}

impl<CTX: ContextTr, P: PrecompileProvider<CTX, Output = InterpreterResult>> PrecompileProvider<CTX>
    for WrappedPrecompile<P>
{
    type Output = P::Output;

    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool {
        self.precompile.set_spec(spec.clone());
        self.spec = spec.into();
        true
    }

    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        let key = (self.spec, inputs.input.bytes(context), inputs.gas_limit);

        // get the result if it exists
        if let Some(precompiles) = self.cache.lock().get_mut(&inputs.target_address) {
            if let Some(result) = precompiles.get(&key) {
                inc_precompile_cache_hits();
                return result.clone().map(Some);
            }
        }

        inc_precompile_cache_misses();

        // call the precompile if cache miss
        let output = self.precompile.run(context, inputs);

        if let Some(output) = output.clone().transpose() {
            // insert the result into the cache
            self.cache
                .lock()
                .entry(inputs.target_address)
                .or_insert(PrecompileResultCache::new(NonZeroUsize::new(2048).unwrap()))
                .put(key, output);
        }

        output
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        self.precompile.warm_addresses()
    }

    fn contains(&self, address: &Address) -> bool {
        self.precompile.contains(address)
    }
}
