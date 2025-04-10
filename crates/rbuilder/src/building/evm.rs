use crate::building::precompile_cache::{PrecompileCache, WrappedPrecompile};
use parking_lot::Mutex;
use reth_evm::{eth::EthEvmContext, EthEvm, Evm, EvmEnv};
use revm::{
    context::{
        result::{EVMError, HaltReason},
        TxEnv,
    },
    handler::EthPrecompiles,
    inspector::NoOpInspector,
    interpreter::interpreter::EthInterpreter,
    primitives::hardfork::SpecId,
    Context, Database, Inspector, MainBuilder, MainContext,
};
use std::sync::Arc;

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

    fn create_evm<DB>(&self, db: DB, env: EvmEnv) -> Self::Evm<DB, NoOpInspector>
    where
        DB: Database<Error: Send + Sync + 'static>;

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

#[derive(Debug, Clone, Default)]
pub struct EthCachedEvmFactory {
    cache: Arc<Mutex<PrecompileCache>>,
}

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
        EthEvm::new(
            Context::mainnet()
                .with_block(env.block_env)
                .with_cfg(env.cfg_env)
                .with_db(db)
                .build_mainnet_with_inspector(NoOpInspector {})
                .with_precompiles(WrappedPrecompile::new(
                    EthPrecompiles::default(),
                    self.cache.clone(),
                )),
            false,
        )
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
