use std::{cmp::Ordering, sync::Arc};

use revm_primitives::U256;

use crate::primitives::{SimValue, SimulatedOrder};

/// Trait to specify how we prioritize orders (eg: which we try first when are building blocks)
pub trait OrderPriority: Ord + Clone + std::fmt::Debug + Send + Sync {
    fn new(order: Arc<SimulatedOrder>) -> Self;
    /// Compares a new execution new_sim_value against the original_sim_value. Returns if it's considered a "good" execution or the profit (or any specific criteria) was too low.
    fn simulation_too_low(original_sim_value: &SimValue, new_sim_value: &SimValue) -> bool;
}

/// Any execution giving less that this might be rejected.
const MIN_SIM_RESULT_PERCENTAGE: u64 = 95;

/// Generic func for gas price or profit. May change in the future.
fn new_sim_value_too_low(original_sim: U256, new_sim: U256) -> bool {
    new_sim * U256::from(100) < (original_sim * U256::from(MIN_SIM_RESULT_PERCENTAGE))
}

// @TODO: Make macro!

///////////////////////////////
/// MevGasPrice
///////////////////////////////

#[derive(Debug, Clone)]
pub struct OrderMevGasPricePriority {
    order: Arc<SimulatedOrder>,
}

impl OrderPriority for OrderMevGasPricePriority {
    fn new(order: Arc<SimulatedOrder>) -> Self {
        Self { order }
    }

    fn simulation_too_low(original_sim_value: &SimValue, new_sim_value: &SimValue) -> bool {
        new_sim_value_too_low(
            original_sim_value.mev_gas_price,
            new_sim_value.mev_gas_price,
        )
    }
}

#[inline]
fn mev_gas_price_eq(a: &SimulatedOrder, b: &SimulatedOrder) -> bool {
    a.sim_value.mev_gas_price == b.sim_value.mev_gas_price
}

#[inline]
fn mev_gas_price_cmp(a: &SimulatedOrder, b: &SimulatedOrder) -> Ordering {
    a.sim_value.mev_gas_price.cmp(&b.sim_value.mev_gas_price)
}

impl PartialEq for OrderMevGasPricePriority {
    fn eq(&self, other: &Self) -> bool {
        mev_gas_price_eq(&self.order, &other.order)
    }
}

impl Eq for OrderMevGasPricePriority {}

impl PartialOrd for OrderMevGasPricePriority {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderMevGasPricePriority {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        mev_gas_price_cmp(&self.order, &other.order)
    }
}

///////////////////////////////
/// MaxProfit
///////////////////////////////

#[derive(Debug, Clone)]
pub struct OrderMaxProfitPriority {
    order: Arc<SimulatedOrder>,
}

impl OrderPriority for OrderMaxProfitPriority {
    fn new(order: Arc<SimulatedOrder>) -> Self {
        Self { order }
    }

    fn simulation_too_low(original_sim_value: &SimValue, new_sim_value: &SimValue) -> bool {
        new_sim_value_too_low(
            original_sim_value.coinbase_profit,
            new_sim_value.coinbase_profit,
        )
    }
}

#[inline]
fn max_profit_eq(a: &SimulatedOrder, b: &SimulatedOrder) -> bool {
    a.sim_value.coinbase_profit == b.sim_value.coinbase_profit
}

#[inline]
fn max_profit_cmp(a: &SimulatedOrder, b: &SimulatedOrder) -> Ordering {
    a.sim_value
        .coinbase_profit
        .cmp(&b.sim_value.coinbase_profit)
}

impl PartialEq for OrderMaxProfitPriority {
    fn eq(&self, other: &Self) -> bool {
        max_profit_eq(&self.order, &other.order)
    }
}

impl Eq for OrderMaxProfitPriority {}

impl PartialOrd for OrderMaxProfitPriority {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderMaxProfitPriority {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        max_profit_cmp(&self.order, &other.order)
    }
}
