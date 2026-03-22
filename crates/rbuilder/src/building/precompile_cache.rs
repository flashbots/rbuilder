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
        // Skip the cache entirely — profiling shows 0 hits across 177K calls.
        // The cache key includes gas_limit which varies per call, preventing hits.
        // The Bytes clone + mutex lock/unlock overhead is pure waste.
        self.precompile.run(context, inputs)
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        self.precompile.warm_addresses()
    }

    fn contains(&self, address: &Address) -> bool {
        self.precompile.contains(address)
    }
}
