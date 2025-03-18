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
        preconf_ordering: Option<u64>,
        preconf_bid_price: Option<u64>,
    ) -> SimulatedOrder {
        let mut ordering = None;
        if preconf_ordering.is_some() {
            ordering = Some(U256::from(preconf_ordering.unwrap()));
        }
        let mut bid_price = None;
        if preconf_bid_price.is_some() {
            bid_price = Some(U256::from(preconf_bid_price.unwrap()));
        }
        let sim_value = SimValue {
            coinbase_profit: U256::from(coinbase_profit),
            mev_gas_price: U256::from(mev_gas_price),
            preconf_ordering: ordering,
            preconf_bid_price: bid_price,
            ..Default::default()
        };
        SimulatedOrder {
            order,
            sim_value,
            prev_order: None,
            used_state_trace: None,
        }
    }
}
