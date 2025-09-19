use std::sync::Arc;

use alloy_primitives::U256;

use rbuilder_primitives::{AccountNonce, Order, SimValue, SimulatedOrder};

/// TestDataGenerator for Orders
#[derive(Default)]
pub struct TestDataGenerator {
    pub base: rbuilder_primitives::TestDataGenerator,
}

impl TestDataGenerator {
    pub fn create_account_nonce(&mut self, nonce: u64) -> AccountNonce {
        AccountNonce {
            nonce,
            account: self.base.create_address(),
        }
    }

    pub fn create_sim_order(
        &self,
        order: Order,
        coinbase_profit: u64,
        mev_gas_price: u64,
    ) -> Arc<SimulatedOrder> {
        let sim_value =
            SimValue::new_test_no_gas(U256::from(coinbase_profit), U256::from(mev_gas_price));

        Arc::new(SimulatedOrder {
            order,
            sim_value,
            used_state_trace: None,
        })
    }
}
