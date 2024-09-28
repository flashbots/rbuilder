use revm::{
    interpreter::{CallInputs, CallOutcome, Interpreter},
    primitives::{B256, U256, Address},
    Database, EvmContext, Inspector,
};
use revmc_toolkit_load::{
    RevmcExtCtxExtTrait, 
    RevmcExtCtx, 
    EvmCompilerFn, 
    Touches,
};
use super::evm_inspector::{
    RBuilderEVMInspector, 
    UsedStateEVMInspector,
};


pub struct RBuilderRevmcInspectorExtCtx<'a, 'b> where 'a: 'b {
    evm_inspector: &'b mut RBuilderEVMInspector<'a>,
    pub revmc_ext_ctx: Option<RevmcExtCtx>,
}

impl<'a, 'b> RBuilderRevmcInspectorExtCtx<'a, 'b> {
    pub fn new(
        evm_inspector: &'b mut RBuilderEVMInspector<'a>,
        revmc_ext_ctx: Option<RevmcExtCtx>,
    ) -> Self {
        Self {
            evm_inspector,
            revmc_ext_ctx,
        }
    }

}

impl<'a, 'b, DB> Inspector<DB> for RBuilderRevmcInspectorExtCtx<'a, 'b>
where
    DB: Database,
    UsedStateEVMInspector<'a>: Inspector<DB>,
{
    #[inline]
    fn step(&mut self, interp: &mut Interpreter, data: &mut EvmContext<DB>) {
        self.evm_inspector.step(interp, data);
    }

    #[inline]
    fn call(
        &mut self,
        context: &mut EvmContext<DB>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        self.evm_inspector.call(context, inputs)
    }

    #[inline]
    fn create_end(
        &mut self,
        context: &mut EvmContext<DB>,
        inputs: &revm::interpreter::CreateInputs,
        outcome: revm::interpreter::CreateOutcome,
    ) -> revm::interpreter::CreateOutcome {
        self.evm_inspector.create_end(context, inputs, outcome)
    }

    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.evm_inspector.selfdestruct(contract, target, value)
    }
}

// todo: improve this - move away from if let statements
impl RevmcExtCtxExtTrait for RBuilderRevmcInspectorExtCtx<'_, '_> {
    fn get_function(&self, bytecode_hash: B256) -> Option<EvmCompilerFn> {
        if let Some(revmc_ext_ctx) = &self.revmc_ext_ctx {
            revmc_ext_ctx.get_function(bytecode_hash)
        } else {
            None
        }
    }
    fn register_touch(&mut self, address: Address, non_native: bool) {
        if let Some(revmc_ext_ctx) = &mut self.revmc_ext_ctx {
            revmc_ext_ctx.register_touch(address, non_native);
        }
    }
    fn touches(&self) -> Option<&Touches> {
        if let Some(revmc_ext_ctx) = &self.revmc_ext_ctx {
            revmc_ext_ctx.touches.as_ref()
        } else {
            None
        }
    }
}
