use crate::building::precompile_cache::{PrecompileCache, WrappedPrecompile};
use alloy_evm::Database;
use parking_lot::Mutex;
use reth_evm::{
    eth::EthEvmContext, EthEvm, EthEvmFactory, Evm, EvmEnv, EvmFactory as RethEvmFactory,
};
use revm::{
    context::{
        result::{EVMError, HaltReason},
        TxEnv,
    },
    handler::EthPrecompiles,
    inspector::NoOpInspector,
    interpreter::interpreter::EthInterpreter,
    primitives::hardfork::SpecId,
    Inspector,
};
use std::sync::Arc;

/// Custom trait to abstract over EVM construction with a cleaner and more concrete
/// interface than the `Evm` trait from `alloy-revm`.
///
/// # Motivation
///
/// The `alloy_revm::Evm` trait comes with a large number of associated types and trait
/// bounds. This new `EvmFactory` trait is designed to encapsulate those complexities,
/// providing an EVM interface less dependent on `alloy-revm` crate.
///
/// It is particularly useful in reducing trait bound noise in other parts of the codebase
/// (i.e. `execute_evm` in `order_commit`), and improves modularity.
///
/// See [`EthCachedEvmFactory`] for an implementation that integrates precompile
/// caching and uses `reth_evm::EthEvm` internally.
pub trait EvmFactory {
    type Evm<DB, I>: Evm<
        DB = DB,
        Tx = TxEnv,
        HaltReason = HaltReason,
        Error = EVMError<DB::Error>,
        Spec = SpecId,
    >
    where
        DB: Database<Error: Send + Sync + 'static>,
        I: Inspector<EthEvmContext<DB>>;

    /// Create an EVM instance with default (no-op) inspector.
    fn create_evm<DB>(&self, db: DB, env: EvmEnv) -> Self::Evm<DB, NoOpInspector>
    where
        DB: Database<Error: Send + Sync + 'static>;

    /// Create an EVM instance with a provided inspector.
    fn create_evm_with_inspector<DB, I>(
        &self,
        db: DB,
        env: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I>
    where
        DB: Database<Error: Send + Sync + 'static>,
        I: Inspector<EthEvmContext<DB>, EthInterpreter>;
}

/// EVM factory used by the block building code for the chain this binary was
/// compiled for. See [`crate::chain`].
#[cfg(not(feature = "arc"))]
pub type ChainCachedEvmFactory = EthCachedEvmFactory;
#[cfg(feature = "arc")]
pub type ChainCachedEvmFactory = arc_factory::ArcCachedEvmFactory;

/// Creates the chain-appropriate EVM factory for the building code.
pub fn create_chain_evm_factory(
    chain_spec: &std::sync::Arc<crate::chain::ChainSpec>,
) -> ChainCachedEvmFactory {
    #[cfg(not(feature = "arc"))]
    {
        let _ = chain_spec;
        EthCachedEvmFactory::default()
    }
    #[cfg(feature = "arc")]
    {
        arc_factory::ArcCachedEvmFactory::new(chain_spec.clone())
    }
}

#[derive(Debug, Clone, Default)]
pub struct EthCachedEvmFactory {
    evm_factory: EthEvmFactory,
    cache: Arc<Mutex<PrecompileCache>>,
}

/// Implementation of the `EvmFactory` trait for `EthCachedEvmFactory`.
///
/// This implementation uses `reth_evm::EthEvm` internally and provides a concrete
/// type for the `Evm` trait.
///
/// It also integrates precompile caching using the [`PrecompileCache`] and
/// [`WrappedPrecompile`] types.
impl EvmFactory for EthCachedEvmFactory {
    type Evm<DB, I>
        = EthEvm<DB, I, WrappedPrecompile<EthPrecompiles>>
    where
        DB: Database<Error: Send + Sync + 'static>,
        I: Inspector<EthEvmContext<DB>>;

    fn create_evm<DB>(&self, db: DB, env: EvmEnv) -> Self::Evm<DB, NoOpInspector>
    where
        DB: Database<Error: Send + Sync + 'static>,
    {
        let evm = self
            .evm_factory
            .create_evm(db, env)
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
        I: Inspector<EthEvmContext<DB>, EthInterpreter>,
    {
        EthEvm::new(
            self.create_evm(db, input)
                .into_inner()
                .with_inspector(inspector),
            true,
        )
    }
}

#[cfg(feature = "arc")]
mod arc_factory {
    use super::{Database, EthEvmContext, EvmEnv, EvmFactory, RethEvmFactory};
    use crate::chain::ChainSpec;
    use arc_evm::ArcEvmFactory;
    use revm::{
        inspector::NoOpInspector, interpreter::interpreter::EthInterpreter, Inspector,
    };
    use std::sync::Arc;

    /// EVM factory for Arc.
    ///
    /// Unlike [`super::EthCachedEvmFactory`] this does NOT cache precompile
    /// calls: several Arc precompiles (NativeCoinControl, SystemAccounting)
    /// read contract storage, so caching results keyed only by input would
    /// return stale data.
    #[derive(Debug, Clone)]
    pub struct ArcCachedEvmFactory {
        evm_factory: ArcEvmFactory,
    }

    impl ArcCachedEvmFactory {
        pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
            Self {
                evm_factory: ArcEvmFactory::new(chain_spec),
            }
        }
    }

    impl EvmFactory for ArcCachedEvmFactory {
        type Evm<DB, I>
            = <ArcEvmFactory as RethEvmFactory>::Evm<DB, I>
        where
            DB: Database<Error: Send + Sync + 'static>,
            I: Inspector<EthEvmContext<DB>>;

        fn create_evm<DB>(&self, db: DB, env: EvmEnv) -> Self::Evm<DB, NoOpInspector>
        where
            DB: Database<Error: Send + Sync + 'static>,
        {
            self.evm_factory.create_evm(db, env)
        }

        fn create_evm_with_inspector<DB, I>(
            &self,
            db: DB,
            env: EvmEnv,
            inspector: I,
        ) -> Self::Evm<DB, I>
        where
            DB: Database<Error: Send + Sync + 'static>,
            I: Inspector<EthEvmContext<DB>, EthInterpreter>,
        {
            self.evm_factory.create_evm_with_inspector(db, env, inspector)
        }
    }
}
