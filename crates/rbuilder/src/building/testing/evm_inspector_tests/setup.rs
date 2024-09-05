use reth_primitives::{transaction::FillTxEnv, Address, TransactionSignedEcRecovered};
use revm::{inspector_handle_register, primitives::Env};
use revm_primitives::TxEnv;

use crate::building::{
    evm_inspector::{RBuilderEVMInspector, UsedStateTrace},
    testing::test_chain_state::{BlockArgs, NamedAddr, TestChainState, TestContracts, TxArgs},
    BlockState,
};

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
    ) -> eyre::Result<TransactionSignedEcRecovered> {
        let tx_args = TxArgs::new(from, 0).to(to).value(value);
        let tx = self.test_chain.sign_tx(tx_args)?;
        Ok(tx)
    }

    pub fn make_increment_value_tx(
        &self,
        slot: u64,
        current_value: u64,
    ) -> eyre::Result<TransactionSignedEcRecovered> {
        let tx_args = TxArgs::new_increment_value(NamedAddr::User(0), 0, slot, current_value);
        let tx = self.test_chain.sign_tx(tx_args)?;
        Ok(tx)
    }

    pub fn make_deploy_mev_test_tx(&self) -> eyre::Result<TransactionSignedEcRecovered> {
        let mev_test_init_bytecode = TestContracts::load().mev_test_init_bytecode;
        let tx_args = TxArgs::new(NamedAddr::User(0), 0).input(mev_test_init_bytecode.into());
        let tx = self.test_chain.sign_tx(tx_args)?;
        Ok(tx)
    }

    pub fn make_test_read_balance_tx(
        &self,
        read_balance_addr: Address,
        value: u64,
    ) -> eyre::Result<TransactionSignedEcRecovered> {
        let tx_args =
            TxArgs::new_test_read_balance(NamedAddr::User(0), 0, read_balance_addr, value);
        let tx = self.test_chain.sign_tx(tx_args)?;
        Ok(tx)
    }

    pub fn make_test_ephemeral_contract_destruct_tx(
        &self,
        refund_addr: Address,
        value: u64,
    ) -> eyre::Result<TransactionSignedEcRecovered> {
        let tx_args =
            TxArgs::new_test_ephemeral_contract_destruct(NamedAddr::User(0), 0, refund_addr)
                .value(value);
        let tx = self.test_chain.sign_tx(tx_args)?;
        Ok(tx)
    }

    pub fn inspect_tx_without_commit(
        &self,
        tx: TransactionSignedEcRecovered,
    ) -> eyre::Result<UsedStateTrace> {
        let mut used_state_trace = UsedStateTrace::default();
        let mut inspector = RBuilderEVMInspector::new(&tx, Some(&mut used_state_trace));

        // block state
        let state_provider = self.test_chain.provider_factory().latest()?;
        let mut block_state = BlockState::new(state_provider);
        let mut db_ref = block_state.new_db_ref();

        // execute transaction
        {
            let mut tx_env = TxEnv::default();
            tx.as_ref().fill_tx_env(&mut tx_env, tx.signer());
            let mut evm = revm::Evm::builder()
                .with_spec_id(self.test_chain.block_building_context().spec_id)
                .with_env(Box::new(Env {
                    cfg: self
                        .test_chain
                        .block_building_context()
                        .initialized_cfg
                        .cfg_env
                        .clone(),
                    block: self.test_chain.block_building_context().block_env.clone(),
                    tx: tx_env,
                }))
                .with_external_context(&mut inspector)
                .with_db(db_ref.as_mut())
                .append_handler_register(inspector_handle_register)
                .build();
            evm.transact()
                .map_err(|e| eyre::eyre!("execution failure: {:?}", e))?;
        }

        Ok(used_state_trace)
    }
}
