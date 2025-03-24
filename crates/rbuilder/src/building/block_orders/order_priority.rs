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

macro_rules! create_order_priority {
    ($order_priority:ident($cmp:ident $( , $next_cmp:ident )*)<$new_sim_value_too_low_func:ident>) => {
        #[derive(Debug, Clone)]
        pub struct $order_priority {
            order: Arc<SimulatedOrder>,
        }

        impl OrderPriority for $order_priority {
            fn new(order: Arc<SimulatedOrder>) -> Self {
                Self { order }
            }

            fn simulation_too_low(
                original_sim_value: &SimValue,
                new_sim_value: &SimValue,
            ) -> bool {
                $new_sim_value_too_low_func(
                    original_sim_value,
                    new_sim_value,
                )
            }
        }

        impl PartialEq for $order_priority {
            fn eq(&self, other: &Self) -> bool {
                $cmp::eq(&self.order, &other.order)
            }
        }

        impl Eq for $order_priority {}

        impl PartialOrd for $order_priority {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl Ord for $order_priority {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                $cmp::cmp(&self.order, &other.order)
                    $( .then_with(|| $next_cmp::cmp(&self.order, &other.order)) )*
            }
        }
    };
}

/// MevGasPrice
struct OrderMevGasPricePriorityCmp {}
impl OrderMevGasPricePriorityCmp {
    #[inline]
    fn eq(a: &SimulatedOrder, b: &SimulatedOrder) -> bool {
        a.sim_value.mev_gas_price == b.sim_value.mev_gas_price
    }

    #[inline]
    fn cmp(a: &SimulatedOrder, b: &SimulatedOrder) -> Ordering {
        a.sim_value.mev_gas_price.cmp(&b.sim_value.mev_gas_price)
    }
}
#[inline]
fn simulation_too_low_gas_price(original_sim_value: &SimValue, new_sim_value: &SimValue) -> bool {
    new_sim_value_too_low(
        original_sim_value.mev_gas_price,
        new_sim_value.mev_gas_price,
    )
}

/// MaxProfit
struct OrderMaxProfitPriorityCmp {}
impl OrderMaxProfitPriorityCmp {
    #[inline]
    fn eq(a: &SimulatedOrder, b: &SimulatedOrder) -> bool {
        a.sim_value.coinbase_profit == b.sim_value.coinbase_profit
    }

    #[inline]
    fn cmp(a: &SimulatedOrder, b: &SimulatedOrder) -> Ordering {
        a.sim_value
            .coinbase_profit
            .cmp(&b.sim_value.coinbase_profit)
    }
}
#[inline]
fn simulation_too_low_profit(original_sim_value: &SimValue, new_sim_value: &SimValue) -> bool {
    new_sim_value_too_low(
        original_sim_value.coinbase_profit,
        new_sim_value.coinbase_profit,
    )
}

/// OrderType
/// Prioritizes Bundles over Mempool
struct OrderTypeCmp {}
impl OrderTypeCmp {
    #[inline]
    fn eq(a: &SimulatedOrder, b: &SimulatedOrder) -> bool {
        a.order.is_tx() == b.order.is_tx()
    }

    #[inline]
    fn cmp(a: &SimulatedOrder, b: &SimulatedOrder) -> Ordering {
        let a_is_tx = a.order.is_tx();
        let b_is_tx = b.order.is_tx();
        if a_is_tx == b_is_tx {
            Ordering::Equal
        } else if a_is_tx {
            //*a_is_tx && !b_is_tx
            Ordering::Less
        } else {
            //*!a_is_tx && b_is_tx
            Ordering::Greater
        }
    }
}

/// Prioritizes orders with 3 or more txs
struct OrderLengthThreeCmp {}
impl OrderLengthThreeCmp {
    #[inline]
    fn is_long(a: &SimulatedOrder) -> bool {
        a.order.list_txs_len() >= 3
    }

    #[inline]
    fn eq(a: &SimulatedOrder, b: &SimulatedOrder) -> bool {
        Self::is_long(a) == Self::is_long(b)
    }

    #[inline]
    fn cmp(a: &SimulatedOrder, b: &SimulatedOrder) -> Ordering {
        let a_is_long = Self::is_long(a);
        let b_is_long = Self::is_long(b);
        if a_is_long == b_is_long {
            Ordering::Equal
        } else if a_is_long {
            //*a_is_long && !b_is_long
            Ordering::Greater
        } else {
            //*!a_is_long && b_is_long
            Ordering::Less
        }
    }
}

/// Breaks ties deterministically if all other orderings gave the same result
struct OrderIDCmp {}
impl OrderIDCmp {
    #[inline]
    fn cmp(a: &SimulatedOrder, b: &SimulatedOrder) -> Ordering {
        a.id().cmp(&b.id())
    }
}

create_order_priority!(OrderMevGasPricePriority(OrderMevGasPricePriorityCmp, OrderIDCmp)<simulation_too_low_gas_price>);
create_order_priority!(OrderMaxProfitPriority(OrderMaxProfitPriorityCmp, OrderIDCmp)<simulation_too_low_profit>);
create_order_priority!(OrderTypePriority(OrderTypeCmp,OrderMaxProfitPriorityCmp, OrderIDCmp)<simulation_too_low_profit>);
create_order_priority!(OrderLengthThreeMaxProfitPriority(OrderLengthThreeCmp,OrderMaxProfitPriorityCmp, OrderIDCmp)<simulation_too_low_profit>);
create_order_priority!(OrderLengthThreeMevGasPricePriority(OrderLengthThreeCmp,OrderMevGasPricePriorityCmp, OrderIDCmp)<simulation_too_low_profit>);
