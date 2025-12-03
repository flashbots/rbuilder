use ahash::HashMap;
use derivative::Derivative;

use crate::building::builders::block_building_helper::BiddableUnfinishedBlock;

/// BestBlockFromAlgorithms maintains last block by each algorithm
/// When new block is created we choose best (by profit) block from last blocks produced by each algorithm.
#[derive(Derivative, Default)]
#[derivative(Debug)]
pub struct BestBlockFromAlgorithms {
    #[derivative(Debug = "ignore")]
    last_block_by_algorithm: HashMap<String, BiddableUnfinishedBlock>,
    last_best_block_hash: u64,
}

impl BestBlockFromAlgorithms {
    pub fn update_with_new_block(
        &mut self,
        unfinished_block: BiddableUnfinishedBlock,
    ) -> Option<BiddableUnfinishedBlock> {
        self.last_block_by_algorithm.insert(
            unfinished_block.block.builder_name().to_string(),
            unfinished_block,
        );
        let last_best_block = self
            .last_block_by_algorithm
            .values()
            .max_by_key(|bb| bb.true_block_value)
            .unwrap();
        let best_block_hash = last_best_block
            .block
            .built_block_trace()
            .transactions_hash();
        if self.last_best_block_hash == best_block_hash {
            None
        } else {
            self.last_best_block_hash = best_block_hash;
            Some(last_best_block.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{I256, U256};

    use crate::building::{
        builders::mock_block_building_helper::MockBlockBuildingHelper, ExecutionResult,
        TransactionExecutionInfo,
    };

    use rbuilder_primitives::{AccountNonce, BlockSpace, Order};

    use super::*;

    struct TestDataGenerator {
        base: rbuilder_primitives::TestDataGenerator,
    }
    impl TestDataGenerator {
        fn new() -> Self {
            Self {
                base: Default::default(),
            }
        }

        /// Creates a unique block with true_block_value / builder_name
        fn create_block(
            &mut self,
            true_block_value: U256,
            builder_name: &str,
        ) -> BiddableUnfinishedBlock {
            let mut block = MockBlockBuildingHelper::new(true_block_value)
                .with_builder_name(builder_name.to_string());
            let order = self.base.create_mempool_tx(AccountNonce::default());
            let tx = order.tx_with_blobs.clone();
            // Give unique identity
            block
                .built_block_trace_mut_ref()
                .included_orders
                .push(ExecutionResult {
                    coinbase_profit: Default::default(),
                    inplace_sim: Default::default(),
                    space_used: Default::default(),
                    order: Order::Tx(order),
                    tx_infos: vec![TransactionExecutionInfo {
                        tx,
                        receipt: Default::default(),
                        space_used: BlockSpace::default(),
                        coinbase_profit: I256::ZERO,
                    }],
                    original_order_ids: Default::default(),
                    nonces_updated: Default::default(),
                    paid_kickbacks: Default::default(),
                    delayed_kickback: None,
                });
            BiddableUnfinishedBlock::new(Box::new(block)).unwrap()
        }
    }

    const NAME_1: &str = "NAME1";
    const NAME_2: &str = "NAME2";
    const LOW_VAL: u64 = 10;
    const HIGH_VAL: u64 = 100;

    #[test]
    fn first_block() {
        let mut data_gen = TestDataGenerator::new();
        let mut state = BestBlockFromAlgorithms::default();
        let block = data_gen.create_block(U256::from(LOW_VAL), NAME_1);
        let best_block = state.update_with_new_block(block.clone());
        assert_eq!(block.true_block_value, best_block.unwrap().true_block_value);
    }

    ///Send block and then send a better one from other builder, the new one should win.
    #[test]
    fn better_block_not_same_as_winning_builder() {
        let mut data_gen = TestDataGenerator::new();
        let mut state = BestBlockFromAlgorithms::default();
        let block_low = data_gen.create_block(U256::from(LOW_VAL), NAME_1);
        let block_high = data_gen.create_block(U256::from(HIGH_VAL), NAME_2);
        let _ = state.update_with_new_block(block_low);
        let best_block = state.update_with_new_block(block_high.clone());
        assert_eq!(
            block_high.true_block_value,
            best_block.unwrap().true_block_value
        );
    }

    ///Send block and then send a better one from the same builder, the new one should win.
    #[test]
    fn better_block_same_winning_builder() {
        let mut data_gen = TestDataGenerator::new();
        let mut state = BestBlockFromAlgorithms::default();
        let block_low = data_gen.create_block(U256::from(LOW_VAL), NAME_1);
        let block_high = data_gen.create_block(U256::from(HIGH_VAL), NAME_1);
        let _ = state.update_with_new_block(block_low);
        let best_block = state.update_with_new_block(block_high.clone());
        assert_eq!(
            block_high.true_block_value,
            best_block.unwrap().true_block_value
        );
    }

    /// Send block and then send a worse one from the same builder, should lower the bid
    #[test]
    fn worse_block_same_winning_builder() {
        let mut data_gen = TestDataGenerator::new();
        let mut state = BestBlockFromAlgorithms::default();
        let block_high = data_gen.create_block(U256::from(HIGH_VAL), NAME_1);
        let block_low = data_gen.create_block(U256::from(LOW_VAL), NAME_1);

        let _ = state.update_with_new_block(block_high);
        let best_block = state.update_with_new_block(block_low.clone());
        assert_eq!(
            block_low.true_block_value,
            best_block.unwrap().true_block_value
        );
    }

    /// Send block and then send a worse one from other builder, should not bid
    #[test]
    fn worse_block_not_same_as_winning_builder() {
        let mut data_gen = TestDataGenerator::new();
        let mut state = BestBlockFromAlgorithms::default();
        let block_high = data_gen.create_block(U256::from(HIGH_VAL), NAME_1);
        let block_low = data_gen.create_block(U256::from(LOW_VAL), NAME_2);

        let _ = state.update_with_new_block(block_high);
        let best_block = state.update_with_new_block(block_low.clone());
        assert!(best_block.is_none());
    }

    /// Send the same winning block twice, second should be ignored
    #[test]
    fn exact_same_block_winning_builder() {
        let mut data_gen = TestDataGenerator::new();
        let mut state = BestBlockFromAlgorithms::default();
        let block = data_gen.create_block(U256::from(LOW_VAL), NAME_1);

        let _ = state.update_with_new_block(block.clone());
        let best_block = state.update_with_new_block(block);
        assert!(best_block.is_none());
    }
}
