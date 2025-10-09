use crate::building::{
    cached_reads::LocalCachedReads,
    evm::EvmFactory,
    testing::test_chain_state::{BlockArgs, NamedAddr, TestChainState, TestContracts, TxArgs},
    BlockState,
};
use alloy_primitives::Address;
use rbuilder_primitives::evm_inspector::{RBuilderEVMInspector, UsedStateTrace};
use reth_evm::Evm;
use reth_primitives::{Recovered, TransactionSigned};

#[derive(Debug)]
pub struct TestSetup {
    test_chain: TestChainState,
}

impl TestSetup {
    pub fn new() -> eyre::Result<Self> {
        Ok(Self {
            test_chain: TestChainState::new(BlockArgs::default())?,
        })
    }

    pub fn named_address(&self, named_addr: NamedAddr) -> eyre::Result<Address> {
        self.test_chain.named_address(named_addr)
    }

    pub fn test_contract_address(&self) -> eyre::Result<Address> {
        self.test_chain.named_address(NamedAddr::MevTest)
    }

    pub fn make_transfer_tx(
        &self,
        from: NamedAddr,
        to: NamedAddr,
        value: u64,
    ) -> eyre::Result<Recovered<TransactionSigned>> {
        let tx_args = TxArgs::new(from, 0).to(to).value(value);
        let tx = self.test_chain.sign_tx(tx_args)?;
        Ok(tx)
    }

    pub fn make_increment_value_tx(
        &self,
        slot: u64,
        current_value: u64,
    ) -> eyre::Result<Recovered<TransactionSigned>> {
        let tx_args = TxArgs::new_increment_value(NamedAddr::User(0), 0, slot, current_value);
        let tx = self.test_chain.sign_tx(tx_args)?;
        Ok(tx)
    }

    pub fn make_deploy_mev_test_tx(&self) -> eyre::Result<Recovered<TransactionSigned>> {
        let mev_test_init_bytecode = TestContracts::load().mev_test_init_bytecode;
        let tx_args = TxArgs::new(NamedAddr::User(0), 0).input(mev_test_init_bytecode.into());
        let tx = self.test_chain.sign_tx(tx_args)?;
        Ok(tx)
    }

    pub fn make_test_read_balance_tx(
        &self,
        read_balance_addr: Address,
        value: u64,
    ) -> eyre::Result<Recovered<TransactionSigned>> {
        let tx_args =
            TxArgs::new_test_read_balance(NamedAddr::User(0), 0, read_balance_addr, value);
        let tx = self.test_chain.sign_tx(tx_args)?;
        Ok(tx)
    }

    pub fn make_test_ephemeral_contract_destruct_tx(
        &self,
        refund_addr: Address,
        value: u64,
    ) -> eyre::Result<Recovered<TransactionSigned>> {
        let tx_args =
            TxArgs::new_test_ephemeral_contract_destruct(NamedAddr::User(0), 0, refund_addr)
                .value(value);
        let tx = self.test_chain.sign_tx(tx_args)?;
        Ok(tx)
    }

    pub fn inspect_tx_without_commit(
        &self,
        tx: Recovered<TransactionSigned>,
    ) -> eyre::Result<UsedStateTrace> {
        let mut used_state_trace = UsedStateTrace::default();
        let mut inspector = RBuilderEVMInspector::new(&tx, Some(&mut used_state_trace));
        let mut local_cached_reads = LocalCachedReads::default();

        // block state
        let state_provider = self.test_chain.provider_factory().latest()?;
        let mut block_state = BlockState::new(state_provider);
        let mut db_ref = block_state.new_db_ref(
            &self.test_chain.block_building_context().shared_cached_reads,
            &mut local_cached_reads,
        );

        // execute transaction
        {
            let ctx = self.test_chain.block_building_context();
            let mut evm = ctx.evm_factory.create_evm_with_inspector(
                db_ref.as_mut(),
                ctx.evm_env.clone(),
                &mut inspector,
            );
            evm.transact(&tx)
                .map_err(|e| eyre::eyre!("execution failure: {:?}", e))?;
        }

        Ok(used_state_trace)
    }
}
