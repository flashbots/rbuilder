//! This module describes state that is available in the test setup
//!
//! test setup creates fake state with various and block (configurable with BlockArgs)
//! test setup is used to build orders and commit them
use crate::{
    building::{
        testing::test_chain_state::{BlockArgs, NamedAddr, TestChainState, TxArgs},
        BlockState, ExecutionError, ExecutionResult, OrderErr, PartialBlock,
    },
    primitives::{
        order_builder::OrderBuilder, BundleReplacementData, OrderId, Refund, RefundConfig,
        SimulatedOrder, TransactionSignedEcRecoveredWithBlobs, TxRevertBehavior,
    },
};
use alloy_primitives::{Address, TxHash};
use reth_payload_builder::database::CachedReads;
use revm::db::BundleState;

pub enum NonceValue {
    /// Fixed value
    Fixed(u64),
    /// Relative value to current nonce (eg: Relative(0) is the current nonce)
    Relative(u64),
}

#[derive(Debug)]
pub struct TestSetup {
    partial_block: PartialBlock<()>,
    order_builder: OrderBuilder,
    bundle_state: Option<BundleState>,
    cached_reads: Option<CachedReads>,
    test_chain: TestChainState,
}

impl TestSetup {
    pub fn gen_test_setup(block_args: BlockArgs) -> eyre::Result<Self> {
        Ok(Self {
            partial_block: PartialBlock::new(true, None),
            order_builder: OrderBuilder::None,
            bundle_state: None,
            cached_reads: None,
            test_chain: TestChainState::new(block_args)?,
        })
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
        let tx_hash = tx.hash;
        self.order_builder.add_tx(
            TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx).unwrap(),
            revert_behavior,
        );
        Ok(tx_hash)
    }

    fn add_tx(&mut self, args: TxArgs, revert_behavior: TxRevertBehavior) -> eyre::Result<TxHash> {
        let tx = self.test_chain.sign_tx(args)?;
        let tx_hash = tx.hash;
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
        let state_provider = self.test_chain.provider_factory().latest()?;
        let mut block_state = BlockState::new(state_provider)
            .with_bundle_state(self.bundle_state.take().unwrap_or_default())
            .with_cached_reads(self.cached_reads.take().unwrap_or_default());

        let sim_order = SimulatedOrder {
            order: self.order_builder.build_order(),
            sim_value: Default::default(),
            prev_order: Default::default(),
            used_state_trace: Default::default(),
        };

        let result = self.partial_block.commit_order(
            &sim_order,
            self.test_chain.block_building_context(),
            &mut block_state,
        )?;

        let (cached_reads, bundle_state, _) = block_state.into_parts();
        self.cached_reads = Some(cached_reads);
        self.bundle_state = Some(bundle_state);

        Ok(result)
    }

    pub fn commit_order_ok(&mut self) -> ExecutionResult {
        let res = self.try_commit_order().expect("Failed to commit order");
        res.expect("Order commit failed")
    }

    pub fn commit_order_err(&mut self, expected_error: &str) {
        let res = self.try_commit_order().expect("Failed to commit order");
        match res {
            Ok(_) => panic!("expected error, result: {:#?}", res),
            Err(err) => {
                if !err
                    .to_string()
                    .to_lowercase()
                    .contains(&expected_error.to_lowercase())
                {
                    panic!("unexpected error: {}, expected: {}", err, expected_error);
                }
            }
        }
    }

    /// Name a little confusing: We expect a ExecutionError::OrderError(OrderError(expected_error))
    pub fn commit_order_err_order_error(&mut self, expected_error: &OrderErr) {
        let res = self.try_commit_order().expect("Failed to commit order");
        match res {
            Ok(_) => panic!("expected error,got ok result: {:#?}", res),
            Err(err) => {
                if let ExecutionError::OrderError(order_error) = err {
                    if *expected_error != order_error {
                        panic!(
                            "unexpected OrderErr error: {}, expected: {}",
                            order_error, expected_error
                        );
                    }
                } else {
                    panic!(
                        "unexpected non OrderErr error: {}, expected: {}",
                        err, expected_error
                    );
                }
            }
        }
    }

    pub fn current_nonce(&self, named_addr: NamedAddr) -> eyre::Result<u64> {
        let state_provider = self.test_chain.provider_factory().latest()?;
        let mut block_state = BlockState::new(state_provider)
            .with_bundle_state(self.bundle_state.clone().unwrap_or_default())
            .with_cached_reads(self.cached_reads.clone().unwrap_or_default());

        Ok(block_state.nonce(self.test_chain.named_address(named_addr)?)?)
    }

    pub fn nonce(&self, named_addr: NamedAddr, nonce_value: NonceValue) -> eyre::Result<u64> {
        match nonce_value {
            NonceValue::Fixed(v) => Ok(v),
            NonceValue::Relative(delta) => Ok(self.current_nonce(named_addr)? + delta),
        }
    }
}
