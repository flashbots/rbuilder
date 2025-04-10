use crate::building::precompile_cache::{PrecompileCache, WrappedPrecompile};
use parking_lot::Mutex;
use reth_evm::{eth::EthEvmContext, EthEvm, EthEvmFactory, EvmEnv, EvmFactory};
use revm::{
    context::{
        result::{EVMError, HaltReason},
        TxEnv,
    },
    handler::EthPrecompiles,
    inspector::NoOpInspector,
    interpreter::interpreter::EthInterpreter,
    primitives::hardfork::SpecId,
    Database, Inspector,
};
use std::sync::Arc;

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
        = EthEvmContext<DB>
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
