//! This module describes state that is available in the test setup
//!
//! test setup creates fake state with various and block (configurable with BlockArgs)
//! test setup is used to build orders and commit them
use crate::building::{
    cached_reads::{LocalCachedReads, SharedCachedReads},
    testing::test_chain_state::{BlockArgs, NamedAddr, TestChainState, TxArgs},
    BlockState, ExecutionError, ExecutionResult, NullPartialBlockExecutionTracer, OrderErr,
    PartialBlock, ThreadBlockBuildingContext,
};
use alloy_primitives::{Address, TxHash};
use rbuilder_primitives::{
    order_builder::OrderBuilder, BundleRefund, BundleReplacementData, OrderId, Refund,
    RefundConfig, SimulatedOrder, TransactionSignedEcRecoveredWithBlobs, TxRevertBehavior,
};
use reth_provider::StateProvider;
use revm::database::states::BundleState;
use std::sync::Arc;

pub enum NonceValue {
    /// Fixed value
    Fixed(u64),
    /// Relative value to current nonce (eg: Relative(0) is the current nonce)
    Relative(u64),
}

#[derive(Debug)]
pub struct TestSetup {
    partial_block: PartialBlock<(), NullPartialBlockExecutionTracer>,
    order_builder: OrderBuilder,
    bundle_state: Option<BundleState>,
    test_chain: TestChainState,
}

impl TestSetup {
    pub fn gen_test_setup(block_args: BlockArgs) -> eyre::Result<Self> {
        Ok(Self {
            partial_block: PartialBlock::new(true),
            order_builder: OrderBuilder::None,
            bundle_state: None,
            test_chain: TestChainState::new(block_args)?,
        })
    }

    /// Return a reference to a partial block.
    pub fn partial_block(&self) -> &PartialBlock<(), NullPartialBlockExecutionTracer> {
        &self.partial_block
    }

    /// Return a mutable reference to the chain state.
    pub fn chain_state_mut(&mut self) -> &mut TestChainState {
        &mut self.test_chain
    }

    pub fn named_address(&self, named_addr: NamedAddr) -> eyre::Result<Address> {
        self.test_chain.named_address(named_addr)
    }

    // Build order methods

    pub fn begin_mempool_tx_order(&mut self) {
        self.order_builder.start_mempool_tx_builder();
    }

    pub fn begin_bundle_order(&mut self, target_block: u64) {
        self.order_builder.start_bundle_builder(target_block);
    }

    pub fn begin_share_bundle_order(&mut self, block: u64, max_block: u64) {
        self.order_builder
            .start_share_bundle_builder(block, max_block);
    }

    // Bundle methods

    pub fn set_bundle_timestamp(&mut self, min_timestamp: Option<u64>, max_timestamp: Option<u64>) {
        self.order_builder
            .set_bundle_timestamp(min_timestamp, max_timestamp);
    }

    pub fn set_bundle_replacement_data(&mut self, replacement_data: BundleReplacementData) {
        self.order_builder
            .set_bundle_replacement_data(replacement_data);
    }

    // Share bundle methods

    pub fn start_inner_bundle(&mut self, can_skip: bool) {
        self.order_builder.start_inner_bundle(can_skip)
    }

    pub fn finish_inner_bundle(&mut self) {
        self.order_builder.finish_inner_bundle()
    }

    pub fn set_inner_bundle_refund(&mut self, refund: Vec<Refund>) {
        self.order_builder.set_inner_bundle_refund(refund)
    }

    pub fn set_bundle_refund(&mut self, refund: BundleRefund) {
        self.order_builder.set_bundle_refund(refund)
    }

    pub fn set_inner_bundle_refund_config(&mut self, refund_config: Vec<RefundConfig>) {
        self.order_builder
            .set_inner_bundle_refund_config(refund_config)
    }

    pub fn set_inner_bundle_original_order_id(&mut self, original_order_id: OrderId) {
        self.order_builder
            .set_inner_bundle_original_order_id(original_order_id)
    }

    /// Adds a tx that does nothing
    /// Can only fail because of nonce or lack of ETH to paid the gas
    pub fn add_null_tx(
        &mut self,
        from: NamedAddr,
        revert_behavior: TxRevertBehavior,
    ) -> eyre::Result<TxHash> {
        self.add_dummy_tx(from, NamedAddr::Dummy, 0, revert_behavior)
    }

    /// Send value 0 from user 0 to user 1, no rev allowed. Current Nonce
    pub fn add_dummy_tx_0_1_no_rev(&mut self) -> eyre::Result<TxHash> {
        self.add_dummy_tx(
            NamedAddr::User(0),
            NamedAddr::User(1),
            0,
            TxRevertBehavior::NotAllowed,
        )
    }

    /// Send value from ->to , uses currentfrom nonce
    pub fn add_dummy_tx(
        &mut self,
        from: NamedAddr,
        to: NamedAddr,
        value: u64,
        revert_behavior: TxRevertBehavior,
    ) -> eyre::Result<TxHash> {
        let args = TxArgs::new(from, self.current_nonce(from)?)
            .to(to)
            .value(value);
        let tx = self.test_chain.sign_tx(args)?;
        let tx_hash = *tx.hash();
        self.order_builder.add_tx(
            TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx).unwrap(),
            revert_behavior,
        );
        Ok(tx_hash)
    }

    fn add_tx(&mut self, args: TxArgs, revert_behavior: TxRevertBehavior) -> eyre::Result<TxHash> {
        let tx = self.test_chain.sign_tx(args)?;
        let tx_hash = *tx.hash();
        self.order_builder.add_tx(
            TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx).unwrap(),
            revert_behavior,
        );
        Ok(tx_hash)
    }

    pub fn add_send_to_coinbase_tx(&mut self, from: NamedAddr, value: u64) -> eyre::Result<TxHash> {
        self.add_tx(
            TxArgs::new_send_to_coinbase(from, self.current_nonce(from)?, value),
            TxRevertBehavior::NotAllowed,
        )
    }

    pub fn add_revert(
        &mut self,
        from: NamedAddr,
        revert_behavior: TxRevertBehavior,
    ) -> eyre::Result<TxHash> {
        self.add_tx(
            TxArgs::new_revert(from, self.current_nonce(from)?),
            revert_behavior,
        )
    }

    /// This transaction will send value to `to` address through the intermediary contract
    pub fn add_mev_test_send_to_tx(
        &mut self,
        from: NamedAddr,
        value_to: NamedAddr,
        value: u64,
        revert_behavior: TxRevertBehavior,
    ) -> eyre::Result<TxHash> {
        let to_addr = self.test_chain.named_address(value_to)?;
        self.add_tx(
            TxArgs::new_send_to(from, self.current_nonce(from)?, value, to_addr),
            revert_behavior,
        )
    }

    /// This transaction will revert if  slot 0's value is not old slot value
    pub fn add_mev_test_increment_value_tx(
        &mut self,
        nonce_value: NonceValue,
        revert_behavior: TxRevertBehavior,
        current_value: u64,
    ) -> eyre::Result<TxHash> {
        let from = NamedAddr::User(0);
        let tx =
            TxArgs::new_increment_value(from, self.nonce(from, nonce_value)?, 0, current_value);
        self.add_tx(tx, revert_behavior)
    }

    /// add_mev_test_increment_value_tx(...TxRevertBehavior::NotAllowed...)
    pub fn add_mev_test_increment_value_tx_no_rev(
        &mut self,
        nonce_value: NonceValue,
        current_value: u64,
    ) -> eyre::Result<TxHash> {
        self.add_mev_test_increment_value_tx(
            nonce_value,
            TxRevertBehavior::NotAllowed,
            current_value,
        )
    }
    fn try_commit_order(&mut self) -> eyre::Result<Result<ExecutionResult, ExecutionError>> {
        let state_provider: Arc<dyn StateProvider> =
            Arc::from(self.test_chain.provider_factory().latest()?);
        let mut local_ctx = ThreadBlockBuildingContext::default();

        let sim_order = SimulatedOrder {
            order: self.order_builder.build_order(),
            sim_value: Default::default(),
            used_state_trace: Default::default(),
        };

        // we commit order twice to test evm caching
        let initial_partial_block = self.partial_block.clone();
        let initial_bundle_state = self.bundle_state.take().unwrap_or_default();

        let mut results = Vec::new();
        for _ in 0..2 {
            let mut block_state = BlockState::new_arc(state_provider.clone())
                .with_bundle_state(initial_bundle_state.clone());

            let mut partial_block = initial_partial_block.clone();

            let result = partial_block.commit_order(
                &sim_order,
                self.test_chain.block_building_context(),
                &mut local_ctx,
                &mut block_state,
                &|_| Ok(()),
            )?;
            results.push(result);
            let (bundle_state, _) = block_state.into_parts();

            self.bundle_state = Some(bundle_state);
            self.partial_block = partial_block
        }

        let second_result = results.pop().unwrap();
        let first_result = results.pop().unwrap();
        if first_result != second_result {
            eyre::bail!("Second order commit differs from the first (caching error) first: {:#?}, second: {:#?}", first_result, second_result);
        }

        Ok(first_result)
    }

    pub fn commit_order_ok(&mut self) -> ExecutionResult {
        let res = self.try_commit_order().expect("Failed to commit order");
        res.expect("Order commit failed")
    }

    pub fn commit_order_err_check_text(&mut self, expected_error: &str) {
        let res = self.try_commit_order().expect("Failed to commit order");
        match res {
            Ok(_) => panic!("expected error, result: {res:#?}"),
            Err(err) => {
                if !err
                    .to_string()
                    .to_lowercase()
                    .contains(&expected_error.to_lowercase())
                {
                    panic!("unexpected error: {err}, expected: {expected_error}");
                }
            }
        }
    }

    /// Name a little confusing: We expect a ExecutionError::OrderError(e) and err_check(e) is ran on the error.
    pub fn commit_order_err_check<F: FnOnce(OrderErr)>(&mut self, err_check: F) {
        let res = self.try_commit_order().expect("Failed to commit order");
        match res {
            Ok(_) => panic!("expected error,got ok result: {res:#?}"),
            Err(err) => {
                if let ExecutionError::OrderError(order_error) = err {
                    err_check(order_error);
                } else {
                    panic!("unexpected non OrderErr error: {err}");
                }
            }
        }
    }

    pub fn current_nonce(&self, named_addr: NamedAddr) -> eyre::Result<u64> {
        let mut local_cached_reads = LocalCachedReads::default();
        let shared_cached_reads = SharedCachedReads::default();

        let state_provider = self.test_chain.provider_factory().latest()?;
        let mut block_state = BlockState::new(state_provider)
            .with_bundle_state(self.bundle_state.clone().unwrap_or_default());

        Ok(block_state.nonce(
            self.test_chain.named_address(named_addr)?,
            &shared_cached_reads,
            &mut local_cached_reads,
        )?)
    }

    pub fn balance(&self, named_addr: NamedAddr) -> eyre::Result<i128> {
        let mut local_cached_reads = LocalCachedReads::default();
        let shared_cached_reads = SharedCachedReads::default();

        let state_provider = self.test_chain.provider_factory().latest()?;
        let mut block_state = BlockState::new(state_provider)
            .with_bundle_state(self.bundle_state.clone().unwrap_or_default());
        Ok(block_state
            .balance(
                self.test_chain.named_address(named_addr)?,
                &shared_cached_reads,
                &mut local_cached_reads,
            )?
            .to())
    }

    pub fn nonce(&self, named_addr: NamedAddr, nonce_value: NonceValue) -> eyre::Result<u64> {
        match nonce_value {
            NonceValue::Fixed(v) => Ok(v),
            NonceValue::Relative(delta) => Ok(self.current_nonce(named_addr)? + delta),
        }
    }
}
