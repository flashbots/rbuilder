use crate::building::precompile_cache::{PrecompileCache, WrappedPrecompile};
use parking_lot::Mutex;
use reth_evm::{
    eth::EthEvmContext, EthEvm, EthEvmFactory, Evm as RethEvm, EvmEnv,
    EvmFactory as RethEvmFactory, IntoTxEnv,
};
use revm::{
    context::{
        result::{EVMError, HaltReason, ResultAndState},
        TxEnv,
    },
    handler::EthPrecompiles,
    interpreter::interpreter::EthInterpreter,
    primitives::hardfork::SpecId,
    Database, Inspector,
};
use std::sync::Arc;

pub trait Evm<DB: Database> {
    fn transact(
        &mut self,
        tx: impl IntoTxEnv<TxEnv>,
    ) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>>;
}

impl<DB, EVM> Evm<DB> for EVM
where
    DB: Database<Error: Send + Sync + 'static>,
    EVM: RethEvm<
        DB = DB,
        Tx = TxEnv,
        Error = EVMError<DB::Error>,
        HaltReason = HaltReason,
        Spec = SpecId,
    >,
{
    fn transact(
        &mut self,
        tx: impl IntoTxEnv<TxEnv>,
    ) -> Result<ResultAndState<HaltReason>, EVMError<DB::Error>> {
        EVM::transact(self, tx)
    }
}

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
    /// Create an EVM instance with default (no-op) inspector.
    fn create_evm<DB>(&self, db: DB, env: EvmEnv) -> impl Evm<DB>
    where
        DB: Database<Error: Send + Sync + 'static>;

    /// Create an EVM instance with a provided inspector.
    fn create_evm_with_inspector<DB, I>(&self, db: DB, env: EvmEnv, inspector: I) -> impl Evm<DB>
    where
        DB: Database<Error: Send + Sync + 'static>,
        I: Inspector<EthEvmContext<DB>, EthInterpreter>;
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
    fn create_evm<DB>(&self, db: DB, env: EvmEnv) -> impl Evm<DB>
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

    fn create_evm_with_inspector<DB, I>(&self, db: DB, env: EvmEnv, inspector: I) -> impl Evm<DB>
    where
        DB: Database<Error: Send + Sync + 'static>,
        I: Inspector<EthEvmContext<DB>, EthInterpreter>,
    {
        let evm = self
            .evm_factory
            .create_evm(db, env)
            .into_inner()
            .with_precompiles(WrappedPrecompile::new(
                EthPrecompiles::default(),
                self.cache.clone(),
            ))
            .with_inspector(inspector);

        EthEvm::new(evm, true)
    }
}
