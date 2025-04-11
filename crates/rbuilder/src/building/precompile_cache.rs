use crate::telemetry::{inc_precompile_cache_hits, inc_precompile_cache_misses};
use ahash::HashMap;
use alloy_primitives::{Address, Bytes};
use derive_more::{Deref, DerefMut};
use lru::LruCache;
use parking_lot::Mutex;
use reth_evm::{eth::EthEvmContext, EthEvm, EthEvmFactory, EvmEnv, EvmFactory};
use revm::{
    context::{
        result::{EVMError, HaltReason},
        BlockEnv, Cfg, CfgEnv, ContextTr, TxEnv,
    },
    handler::{EthPrecompiles, PrecompileProvider},
    inspector::NoOpInspector,
    interpreter::{interpreter::EthInterpreter, InputsImpl, InterpreterResult},
    primitives::hardfork::SpecId,
    Context, Database, Inspector,
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
        address: &Address,
        inputs: &InputsImpl,
        is_static: bool,
        gas_limit: u64,
    ) -> Result<Option<Self::Output>, String> {
        let key = (self.spec, inputs.input.clone(), gas_limit);

        // get the result if it exists
        if let Some(precompiles) = self.cache.lock().get_mut(address) {
            if let Some(result) = precompiles.get(&key) {
                inc_precompile_cache_hits();
                return result.clone().map(Some);
            }
        }

        inc_precompile_cache_misses();

        // call the precompile if cache miss
        let output = self
            .precompile
            .run(context, address, inputs, is_static, gas_limit);

        if let Some(output) = output.clone().transpose() {
            // insert the result into the cache
            self.cache
                .lock()
                .entry(*address)
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

#[derive(Debug, Clone, Default)]
pub struct EthCachedEvmFactory {
    evm_factory: EthEvmFactory,
    cache: Arc<Mutex<PrecompileCache>>,
}

impl EvmFactory for EthCachedEvmFactory {
    type Evm<DB, I>
        = EthEvm<DB, I, WrappedPrecompile<EthPrecompiles>>
    where
        DB: Database<Error: Send + Sync + 'static>,
        I: Inspector<EthEvmContext<DB>>;

    type Context<DB>
        = Context<BlockEnv, TxEnv, CfgEnv, DB>
    where
        DB: Database<Error: Send + Sync + 'static>;

    type Error<DBError>
        = EVMError<DBError>
    where
        DBError: core::error::Error + Send + Sync + 'static;

    type Tx = TxEnv;
    type HaltReason = HaltReason;
    type Spec = SpecId;

    fn create_evm<DB>(&self, db: DB, input: EvmEnv) -> Self::Evm<DB, NoOpInspector>
    where
        DB: Database<Error: Send + Sync + 'static>,
    {
        let evm = self
            .evm_factory
            .create_evm(db, input)
            .into_inner()
            .with_precompiles(WrappedPrecompile::new(
                EthPrecompiles::default(),
                self.cache.clone(),
            ));

        EthEvm::new(evm, false)
    }

    fn create_evm_with_inspector<DB, I>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I>
    where
        DB: Database<Error: Send + Sync + 'static>,
        I: Inspector<Self::Context<DB>, EthInterpreter>,
    {
        EthEvm::new(
            self.create_evm(db, input)
                .into_inner()
                .with_inspector(inspector),
            true,
        )
    }
}
