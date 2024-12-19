use alloy_primitives::U256;

use crate::primitives::{AccountNonce, Order, SimValue, SimulatedOrder};

/// TestDataGenerator for Orders
#[derive(Default)]
pub struct TestDataGenerator {
    pub base: crate::primitives::TestDataGenerator,
}

impl TestDataGenerator {
    pub fn create_account_nonce(&mut self, nonce: u64) -> AccountNonce {
        AccountNonce {
            nonce,
            account: self.base.base.create_address(),
        }
    }

    pub fn create_sim_order(
        &self,
        order: Order,
        coinbase_profit: u64,
        mev_gas_price: u64,
    ) -> SimulatedOrder {
        let sim_value = SimValue {
            coinbase_profit: U256::from(coinbase_profit),
            mev_gas_price: U256::from(mev_gas_price),
            ..Default::default()
        };
        SimulatedOrder {
            order,
            sim_value,
            used_state_trace: None,
        }
    }
}
