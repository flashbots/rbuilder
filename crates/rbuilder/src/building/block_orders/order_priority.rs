use alloy_primitives::U256;
use rbuilder_primitives::{ProfitInfo, SimValue, SimulatedOrder};
use std::{cmp::Ordering, sync::Arc};

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

/// "std::fmt::Debug + Clone + Sync + Send" should not be needed since we never really instantiate one of this (only phantom)
/// but at some point you decide to let the f*cking compiler win and go on with you life.
pub trait ProfitInfoGetter: std::fmt::Debug + Clone + Sync + Send {
    fn get_profit_info(sim_value: &SimValue) -> &ProfitInfo;
}

#[derive(Debug, Clone)]
pub struct FullProfitInfoGetter {}
impl ProfitInfoGetter for FullProfitInfoGetter {
    fn get_profit_info(sim_value: &SimValue) -> &ProfitInfo {
        sim_value.full_profit_info()
    }
}

#[derive(Debug, Clone)]
pub struct NonMempoolProfitInfoGetter {}
impl ProfitInfoGetter for NonMempoolProfitInfoGetter {
    fn get_profit_info(sim_value: &SimValue) -> &ProfitInfo {
        sim_value.non_mempool_profit_info()
    }
}

/// Creates a OrderPriority named order_priority comparing using all the cmp/next_cmp and adding OrderIDCmp at the end.
/// The created OrderPriority is generic on <ProfitInfoGetterType: ProfitInfoGetter>.
/// Usage:
/// create_order_priority!(NamedPriority((Cmp1,uses_getter/plain),(Cmp2,uses_getter/plain))<simulation_too_low_func>);
/// CmpN is an struct implementing:
/// - fn eq(a: &SimulatedOrder, b: &SimulatedOrder) -> bool
/// - fn cmp(a: &SimulatedOrder, b: &SimulatedOrder) -> Ordering
///
/// uses_getter: will propagate <ProfitInfoGetterType> to the type instantiation.
/// plain:  will propagate instantiation CmpN as is.
/// simulation_too_low_func: fn simulation_too_low_func<ProfitInfoGetterType: ProfitInfoGetter>(original_sim_value: &SimValue,new_sim_value: &SimValue).
/// simulation_too_low_func must return true if new_sim_value (generated while building a block) is too low compared to original_sim_value (sim on top of block).
macro_rules! create_order_priority {
    ($order_priority:ident(($cmp:ident, $cmp_needs_generic:ident) $( , ($next_cmp:ident,$next_cmp_needs_generic:ident) )*)<$new_sim_value_too_low_func:ident>) => {
        #[derive(Debug, Clone)]
        pub struct $order_priority <ProfitInfoGetterType: ProfitInfoGetter> {
            order: Arc<SimulatedOrder>,
            _phantom:std::marker::PhantomData<ProfitInfoGetterType>,
        }

        impl<ProfitInfoGetterType: ProfitInfoGetter> OrderPriority for $order_priority<ProfitInfoGetterType> {
            fn new(order: Arc<SimulatedOrder>) -> Self {
                Self { order,_phantom: Default::default()}
            }

            fn simulation_too_low(
                original_sim_value: &SimValue,
                new_sim_value: &SimValue,
            ) -> bool {
                $new_sim_value_too_low_func::<ProfitInfoGetterType>(
                    original_sim_value,
                    new_sim_value,
                )
            }
        }

        impl<ProfitInfoGetterType: ProfitInfoGetter> PartialEq for $order_priority<ProfitInfoGetterType> {
            fn eq(&self, other: &Self) -> bool {
                <$crate::add_getter!($cmp,$cmp_needs_generic)>::eq(&self.order, &other.order)
                $( && <$crate::add_getter!($next_cmp,$next_cmp_needs_generic)>::eq(&self.order, &other.order) )*
                && OrderIDCmp::eq(&self.order, &other.order)
            }
        }

        impl<ProfitInfoGetterType: ProfitInfoGetter> Eq for $order_priority<ProfitInfoGetterType> {}

        impl<ProfitInfoGetterType: ProfitInfoGetter> PartialOrd for $order_priority<ProfitInfoGetterType> {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl<ProfitInfoGetterType: ProfitInfoGetter> Ord for $order_priority<ProfitInfoGetterType> {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                <$crate::add_getter!($cmp,$cmp_needs_generic)>::cmp(&self.order, &other.order)
                    $( .then_with(|| <$crate::add_getter!($next_cmp,$next_cmp_needs_generic)>::cmp(&self.order, &other.order)) )*
                    .then_with(||OrderIDCmp::cmp(&self.order, &other.order))
            }
        }
    };
}

// Simplified helper macro with hard-coded ProfitInfoGetterType
#[macro_export]
macro_rules! add_getter {
    // If $needs_generic is "needs", apply ProfitInfoGetterType
    ($type:ident, uses_getter) => {
        $type<ProfitInfoGetterType>
    };
    // Otherwise, use the type as is
    ($type:ident, plain) => {
        $type
    };
}

/// MevGasPrice
struct OrderMevGasPricePriorityCmp<ProfitInfoGetterType: ProfitInfoGetter>(
    std::marker::PhantomData<ProfitInfoGetterType>,
);

impl<ProfitInfoGetterType: ProfitInfoGetter> OrderMevGasPricePriorityCmp<ProfitInfoGetterType> {
    #[inline]
    fn eq(a: &SimulatedOrder, b: &SimulatedOrder) -> bool {
        ProfitInfoGetterType::get_profit_info(&a.sim_value).mev_gas_price()
            == ProfitInfoGetterType::get_profit_info(&b.sim_value).mev_gas_price()
    }

    #[inline]
    fn cmp(a: &SimulatedOrder, b: &SimulatedOrder) -> Ordering {
        ProfitInfoGetterType::get_profit_info(&a.sim_value)
            .mev_gas_price()
            .cmp(&ProfitInfoGetterType::get_profit_info(&b.sim_value).mev_gas_price())
    }
}
#[inline]
fn simulation_too_low_gas_price<ProfitInfoGetterType: ProfitInfoGetter>(
    original_sim_value: &SimValue,
    new_sim_value: &SimValue,
) -> bool {
    new_sim_value_too_low(
        ProfitInfoGetterType::get_profit_info(original_sim_value).mev_gas_price(),
        ProfitInfoGetterType::get_profit_info(new_sim_value).mev_gas_price(),
    )
}

/// MaxProfit
struct OrderMaxProfitPriorityCmp<ProfitInfoGetterType: ProfitInfoGetter>(
    std::marker::PhantomData<ProfitInfoGetterType>,
);

impl<ProfitInfoGetterType: ProfitInfoGetter> OrderMaxProfitPriorityCmp<ProfitInfoGetterType> {
    #[inline]
    fn eq(a: &SimulatedOrder, b: &SimulatedOrder) -> bool {
        ProfitInfoGetterType::get_profit_info(&a.sim_value).coinbase_profit()
            == ProfitInfoGetterType::get_profit_info(&b.sim_value).coinbase_profit()
    }

    #[inline]
    fn cmp(a: &SimulatedOrder, b: &SimulatedOrder) -> Ordering {
        ProfitInfoGetterType::get_profit_info(&a.sim_value)
            .coinbase_profit()
            .cmp(&ProfitInfoGetterType::get_profit_info(&b.sim_value).coinbase_profit())
    }
}
#[inline]
fn simulation_too_low_profit<ProfitInfoGetterType: ProfitInfoGetter>(
    original_sim_value: &SimValue,
    new_sim_value: &SimValue,
) -> bool {
    new_sim_value_too_low(
        ProfitInfoGetterType::get_profit_info(original_sim_value).coinbase_profit(),
        ProfitInfoGetterType::get_profit_info(new_sim_value).coinbase_profit(),
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
    #[inline]
    fn eq(a: &SimulatedOrder, b: &SimulatedOrder) -> bool {
        a.id().eq(&b.id())
    }
}

create_order_priority!(OrderMevGasPricePriority((OrderMevGasPricePriorityCmp,uses_getter))<simulation_too_low_gas_price>);
create_order_priority!(OrderMaxProfitPriority((OrderMaxProfitPriorityCmp,uses_getter))<simulation_too_low_profit>);
create_order_priority!(OrderTypePriority((OrderTypeCmp,plain),(OrderMaxProfitPriorityCmp,uses_getter))<simulation_too_low_profit>);
create_order_priority!(OrderLengthThreeMaxProfitPriority((OrderLengthThreeCmp,plain),(OrderMaxProfitPriorityCmp,uses_getter))<simulation_too_low_profit>);
create_order_priority!(OrderLengthThreeMevGasPricePriority((OrderLengthThreeCmp,plain),(OrderMevGasPricePriorityCmp,uses_getter))<simulation_too_low_profit>);

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use alloy_primitives::U256;

    use crate::building::order_priority::NonMempoolProfitInfoGetter;
    use rbuilder_primitives::{AccountNonce, BundledTxInfo, Order, SimValue, SimulatedOrder};

    use super::{
        FullProfitInfoGetter, OrderLengthThreeMaxProfitPriority,
        OrderLengthThreeMevGasPricePriority, OrderMaxProfitPriority, OrderMevGasPricePriority,
        OrderPriority, OrderTypePriority,
    };

    enum OrderType {
        MempoolTx,
        BundleLength1,
        BundleLength3,
    }

    #[derive(Default)]
    struct TestContext {
        data_gen: rbuilder_primitives::TestDataGenerator,
    }

    impl TestContext {
        /// dummy
        fn create_nonce(&mut self) -> AccountNonce {
            AccountNonce {
                nonce: Default::default(),
                account: self.data_gen.create_address(),
            }
        }

        /// Super dummy order only used to check the type an length.
        fn create_order(&mut self, order_type: OrderType) -> Order {
            let nonce = self.create_nonce();
            match order_type {
                OrderType::MempoolTx => self.data_gen.create_tx_order(nonce),
                OrderType::BundleLength1 => self.data_gen.create_bundle_multi_tx_order(
                    Default::default(),
                    &[BundledTxInfo {
                        nonce,
                        optional: true,
                    }],
                    None,
                ),
                OrderType::BundleLength3 => self.data_gen.create_bundle_multi_tx_order(
                    Default::default(),
                    &[
                        BundledTxInfo {
                            nonce: nonce.clone(),
                            optional: true,
                        },
                        BundledTxInfo {
                            nonce: nonce.clone(),
                            optional: true,
                        },
                        BundledTxInfo {
                            nonce,
                            optional: true,
                        },
                    ],
                    None,
                ),
            }
        }

        /// Creates a SimulatedOrder with synthetic SimValue and an order of a specific type.
        /// SimValue is not related to the real order execution (there is no chain!!).
        fn create_sim_order(
            &mut self,
            full_profit: u64,
            non_mempool_profit: u64,
            gas: u64,
            order_type: OrderType,
        ) -> Arc<SimulatedOrder> {
            Arc::new(SimulatedOrder {
                order: self.create_order(order_type),
                sim_value: SimValue::new_test(
                    U256::from(full_profit),
                    U256::from(non_mempool_profit),
                    gas,
                ),
                used_state_trace: None,
            })
        }
    }

    const HIGH_PROFIT: u64 = 10_000;
    const MID_PROFIT: u64 = 5_000;
    const LOW_PROFIT: u64 = 1_000;
    const DONT_CARE_GAS: u64 = 100;

    const NORMAL_GAS: u64 = 100;
    /// this gas makes any profit win by mev gas price (LOW_PROFIT/SUPER_LOW_GAS will win to HIGH_PROFIT/NORMAL_GAS)
    const SUPER_LOW_GAS: u64 = 1;

    fn assert_is_less<OrderPriorityType: OrderPriority>(
        sim_order_a: &Arc<SimulatedOrder>,
        sim_order_b: &Arc<SimulatedOrder>,
    ) {
        assert!(
            OrderPriorityType::new(sim_order_a.clone())
                < OrderPriorityType::new(sim_order_b.clone())
        );
    }

    /// Check some orders involving NonMempoolProfitInfoGetter/FullProfitInfoGetter
    #[test]
    fn price_info_getter() {
        let mut ctx = TestContext::default();
        let sim_full_high_non_mempool_low =
            ctx.create_sim_order(HIGH_PROFIT, LOW_PROFIT, DONT_CARE_GAS, OrderType::MempoolTx);
        let sim_non_mempool_high_full_low =
            ctx.create_sim_order(LOW_PROFIT, HIGH_PROFIT, DONT_CARE_GAS, OrderType::MempoolTx);
        assert_is_less::<OrderMaxProfitPriority<NonMempoolProfitInfoGetter>>(
            &sim_full_high_non_mempool_low,
            &sim_non_mempool_high_full_low,
        );
        assert_is_less::<OrderMaxProfitPriority<FullProfitInfoGetter>>(
            &sim_non_mempool_high_full_low,
            &sim_full_high_non_mempool_low,
        );
        assert_is_less::<OrderMevGasPricePriority<NonMempoolProfitInfoGetter>>(
            &sim_full_high_non_mempool_low,
            &sim_non_mempool_high_full_low,
        );
        assert_is_less::<OrderMevGasPricePriority<FullProfitInfoGetter>>(
            &sim_non_mempool_high_full_low,
            &sim_full_high_non_mempool_low,
        );
    }

    /// OrderMevGasPricePriority: Check gas price is not confused by profit.
    #[test]
    fn gas_price() {
        let mut ctx = TestContext::default();
        let sim_high_gas_price_low_profit =
            ctx.create_sim_order(LOW_PROFIT, LOW_PROFIT, SUPER_LOW_GAS, OrderType::MempoolTx);
        let sim_high_profit_low_gas_price =
            ctx.create_sim_order(HIGH_PROFIT, HIGH_PROFIT, NORMAL_GAS, OrderType::MempoolTx);
        assert_is_less::<OrderMevGasPricePriority<FullProfitInfoGetter>>(
            &sim_high_profit_low_gas_price,
            &sim_high_gas_price_low_profit,
        );
    }

    /// OrderTypePriority: bundles wins to mempool
    #[test]
    fn order_type() {
        let mut ctx = TestContext::default();
        let sim_tx_high = ctx.create_sim_order(
            HIGH_PROFIT,
            HIGH_PROFIT,
            DONT_CARE_GAS,
            OrderType::MempoolTx,
        );
        let sim_tx_low =
            ctx.create_sim_order(LOW_PROFIT, LOW_PROFIT, DONT_CARE_GAS, OrderType::MempoolTx);

        let sim_bundle_mid = ctx.create_sim_order(
            MID_PROFIT,
            MID_PROFIT,
            DONT_CARE_GAS,
            OrderType::BundleLength1,
        );
        let sim_bundle_low = ctx.create_sim_order(
            LOW_PROFIT,
            LOW_PROFIT,
            DONT_CARE_GAS,
            OrderType::BundleLength1,
        );
        assert_is_less::<OrderTypePriority<FullProfitInfoGetter>>(&sim_tx_high, &sim_bundle_mid);
        // Tie bundles
        assert_is_less::<OrderTypePriority<FullProfitInfoGetter>>(&sim_bundle_low, &sim_bundle_mid);
        // Tie txs
        assert_is_less::<OrderTypePriority<FullProfitInfoGetter>>(&sim_tx_low, &sim_tx_high);
    }

    /// OrderLengthThreeMaxProfitPriority: size 3 before size < 3 (even if < 3 has good profit)
    #[test]
    fn order_length_three_max_profit_priority() {
        let mut ctx = TestContext::default();
        let sim_size_1_high = ctx.create_sim_order(
            HIGH_PROFIT,
            HIGH_PROFIT,
            DONT_CARE_GAS,
            OrderType::BundleLength1,
        );

        let sim_size_1_low = ctx.create_sim_order(
            LOW_PROFIT,
            LOW_PROFIT,
            DONT_CARE_GAS,
            OrderType::BundleLength1,
        );

        let sim_size_3_mid = ctx.create_sim_order(
            MID_PROFIT,
            MID_PROFIT,
            DONT_CARE_GAS,
            OrderType::BundleLength3,
        );
        let sim_size_3_low = ctx.create_sim_order(
            LOW_PROFIT,
            LOW_PROFIT,
            DONT_CARE_GAS,
            OrderType::BundleLength3,
        );

        assert_is_less::<OrderLengthThreeMaxProfitPriority<FullProfitInfoGetter>>(
            &sim_size_1_high,
            &sim_size_3_low,
        );

        // Tie on size 3, wins profit
        assert_is_less::<OrderLengthThreeMaxProfitPriority<FullProfitInfoGetter>>(
            &sim_size_3_low,
            &sim_size_3_mid,
        );

        // Tie on size 1, wins profit
        assert_is_less::<OrderLengthThreeMaxProfitPriority<FullProfitInfoGetter>>(
            &sim_size_1_low,
            &sim_size_1_high,
        );
    }

    /// OrderLengthThreeMevGasPricePriority
    #[test]
    fn order_length_three_mev_gas_price_priority() {
        let mut ctx = TestContext::default();
        let sim_size_1_high = ctx.create_sim_order(
            MID_PROFIT,
            MID_PROFIT,
            SUPER_LOW_GAS,
            OrderType::BundleLength1,
        );

        let sim_size_1_low =
            ctx.create_sim_order(LOW_PROFIT, LOW_PROFIT, NORMAL_GAS, OrderType::BundleLength1);

        let sim_size_3_mid = ctx.create_sim_order(
            LOW_PROFIT,
            LOW_PROFIT,
            SUPER_LOW_GAS,
            OrderType::BundleLength3,
        );
        let sim_size_3_low =
            ctx.create_sim_order(LOW_PROFIT, LOW_PROFIT, NORMAL_GAS, OrderType::BundleLength3);

        assert_is_less::<OrderLengthThreeMevGasPricePriority<FullProfitInfoGetter>>(
            &sim_size_1_high,
            &sim_size_3_low,
        );

        // Tie on size 3, wins profit
        assert_is_less::<OrderLengthThreeMevGasPricePriority<FullProfitInfoGetter>>(
            &sim_size_3_low,
            &sim_size_3_mid,
        );

        // Tie on size 1, wins profit
        assert_is_less::<OrderLengthThreeMevGasPricePriority<FullProfitInfoGetter>>(
            &sim_size_1_low,
            &sim_size_1_high,
        );
    }
}
