pub mod multi_share_bundle_merger;
mod prioritized_order_store;
mod share_bundle_merger;

#[cfg(test)]
mod order_dumper;
pub mod order_priority;
#[cfg(test)]
mod test_context;
mod test_data_generator;
use std::sync::Arc;

use crate::live_builder::simulation::SimulatedOrderCommand;
use ahash::HashMap;
use rbuilder_primitives::{AccountNonce, OrderId, SimulatedOrder};
use reth_errors::ProviderResult;
use reth_provider::StateProviderBox;

pub use order_priority::OrderPriority;
pub use prioritized_order_store::PrioritizedOrderStore;
pub use test_data_generator::TestDataGenerator;

/// Generic SimulatedOrder sink to add and remove orders.
pub trait SimulatedOrderSink {
    fn insert_order(&mut self, order: Arc<SimulatedOrder>);
    /// if found, returns the removed order
    fn remove_order(&mut self, id: OrderId) -> Option<Arc<SimulatedOrder>>;
    fn remove_orders(
        &mut self,
        orders: impl IntoIterator<Item = OrderId>,
    ) -> Vec<Arc<SimulatedOrder>> {
        let mut result = Vec::new();
        for id in orders {
            if let Some(o) = self.remove_order(id) {
                result.push(o);
            }
        }
        result
    }
}

/// Sends command to sink calling the proper function.
pub fn simulated_order_command_to_sink<SinkType: SimulatedOrderSink>(
    command: SimulatedOrderCommand,
    sink: &mut SinkType,
) {
    match command {
        SimulatedOrderCommand::Simulation(sim_order) => sink.insert_order(sim_order),
        SimulatedOrderCommand::Cancellation(id) => {
            let _ = sink.remove_order(id);
        }
    };
}

/// SimulatedOrderSink that stores all orders + all the adds from last drain_new_orders ONLY if we didn't see removes.
pub struct SimulatedOrderStore {
    /// Id -> order for all orders we manage. Carefully maintained by remove/insert
    orders: HashMap<OrderId, Arc<SimulatedOrder>>,
    /// Stored add commands since last drain_new_orders. If we see a cancel y goes to None.
    /// This could be in another object..
    new_orders: Option<Vec<Arc<SimulatedOrder>>>,
}

impl SimulatedOrderStore {
    pub fn new() -> Self {
        Self {
            orders: Default::default(),
            new_orders: Some(Vec::new()),
        }
    }

    pub fn get_orders(&self) -> Vec<Arc<SimulatedOrder>> {
        self.orders.values().cloned().collect()
    }

    /// Allows to get new adds ONLY if no remove_order was received
    pub fn drain_new_orders(&mut self) -> Option<Vec<Arc<SimulatedOrder>>> {
        self.new_orders.replace(Vec::new())
    }
}

impl Default for SimulatedOrderStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SimulatedOrderSink for SimulatedOrderStore {
    fn insert_order(&mut self, order: Arc<SimulatedOrder>) {
        if let Some(new_orders) = &mut self.new_orders {
            new_orders.push(order.clone());
        }
        self.orders.insert(order.id(), order);
    }

    fn remove_order(&mut self, id: OrderId) -> Option<Arc<SimulatedOrder>> {
        self.new_orders = None;
        self.orders.remove(&id)
    }
}

/// Create block orders struct from simulated orders. Used in the backtest, not practical while live.
pub fn block_orders_from_sim_orders<OrderPriorityType: OrderPriority>(
    sim_orders: &[Arc<SimulatedOrder>],
    state_provider: &StateProviderBox,
) -> ProviderResult<PrioritizedOrderStore<OrderPriorityType>> {
    let mut onchain_nonces = vec![];
    for order in sim_orders {
        for nonce in order.order.nonces() {
            let value = state_provider
                .account_nonce(&nonce.address)?
                .unwrap_or_default();
            onchain_nonces.push(AccountNonce {
                account: nonce.address,
                nonce: value,
            });
        }
    }
    let mut block_orders = PrioritizedOrderStore::<OrderPriorityType>::new(onchain_nonces);

    for order in sim_orders.iter().cloned() {
        block_orders.insert_order(order);
    }

    Ok(block_orders)
}

#[cfg(test)]
mod test {
    use rbuilder_primitives::BundledTxInfo;

    use super::{
        order_priority::{FullProfitInfoGetter, OrderMaxProfitPriority},
        *,
    };
    /// Helper struct for common PrioritizedOrderStore test operations
    /// Works hardcoded on Sorting::MaxProfit since it changes nothing on internal logic
    struct TestContext {
        pub data_gen: TestDataGenerator,
        pub order_pool: PrioritizedOrderStore<OrderMaxProfitPriority<FullProfitInfoGetter>>,
    }

    impl TestContext {
        /// Context with 1 account to send txs from
        pub fn new_1_account(nonce: u64) -> (AccountNonce, Self) {
            let mut data_gen = TestDataGenerator::default();
            let nonce = data_gen.create_account_nonce(nonce);
            (
                nonce.clone(),
                TestContext {
                    data_gen,
                    order_pool:
                        PrioritizedOrderStore::<OrderMaxProfitPriority<FullProfitInfoGetter>>::new(
                            vec![nonce],
                        ),
                },
            )
        }

        /// Context with 2 accounts to send txs from
        pub fn new_2_accounts(
            nonce_1: u64,
            nonce_2: u64,
        ) -> (AccountNonce, AccountNonce, TestContext) {
            let mut data_gen = TestDataGenerator::default();
            let nonce_1 = data_gen.create_account_nonce(nonce_1);
            let nonce_2 = data_gen.create_account_nonce(nonce_2);
            (
                nonce_1.clone(),
                nonce_2.clone(),
                TestContext {
                    data_gen,
                    order_pool:
                        PrioritizedOrderStore::<OrderMaxProfitPriority<FullProfitInfoGetter>>::new(
                            vec![nonce_1, nonce_2],
                        ),
                },
            )
        }

        /// Creates a single tx order with tx_nonce giving tx_profit and adds it to the order_pool
        pub fn create_add_tx_order(
            &mut self,
            tx_nonce: &AccountNonce,
            tx_profit: u64,
        ) -> Arc<SimulatedOrder> {
            let order = self.data_gen.base.create_tx_order(tx_nonce.clone());
            let order = self.data_gen.create_sim_order(order, tx_profit, tx_profit);
            self.order_pool.insert_order(order.clone());
            order
        }

        /// Creates a bundle with multiple orders (like create_add_txt_order). It optionally adds replacement data.
        pub fn create_add_bundle_order(
            &mut self,
            txs_info: &[BundledTxInfo],
            bundle_profit: u64,
        ) -> Arc<SimulatedOrder> {
            let order = self.data_gen.base.create_bundle_multi_tx_order(
                0, // in the context of PrioritizedOrderStore we don't care about the block (it's prefiltered)
                txs_info, None,
            );
            let order = self
                .data_gen
                .create_sim_order(order, bundle_profit, bundle_profit);
            self.order_pool.insert_order(order.clone());
            order
        }

        /// create_add_bundle_order helper for 2 orders
        pub fn create_add_bundle_order_2_txs(
            &mut self,
            tx1_nonce: &AccountNonce,
            tx1_optional: bool,
            tx2_nonce: &AccountNonce,
            tx2_optional: bool,
            bundle_profit: u64,
        ) -> Arc<SimulatedOrder> {
            let txs_info = [
                BundledTxInfo {
                    nonce: tx1_nonce.clone(),
                    optional: tx1_optional,
                },
                BundledTxInfo {
                    nonce: tx2_nonce.clone(),
                    optional: tx2_optional,
                },
            ];
            self.create_add_bundle_order(&txs_info, bundle_profit)
        }

        /// Creates a single tx order giving tx_profit and adds it to the order_pool
        pub fn update_nonce(&mut self, tx_nonce: &AccountNonce, new_nonce: u64) {
            self.order_pool.update_onchain_nonces(&[AccountNonce {
                account: tx_nonce.account,
                nonce: new_nonce,
            }]);
        }

        pub fn assert_pop_order(&mut self, order: &SimulatedOrder) {
            assert_eq!(
                self.order_pool.pop_order().map(|o| o.id()),
                Some(order.id())
            );
        }

        pub fn assert_pop_none(&mut self) {
            assert_eq!(self.order_pool.pop_order().map(|o| o.id()), None);
        }
    }

    #[test]
    /// Tests 2 tx from different accounts, can execute both
    fn test_block_orders_simple() {
        let (nonce_worst_order, nonce_best_order, mut context) = TestContext::new_2_accounts(0, 1);
        let worst_order = context.create_add_tx_order(&nonce_worst_order, 0);
        let best_order = context.create_add_tx_order(&nonce_best_order, 5);
        // we must see first the most profitable order
        context.assert_pop_order(&best_order);
        // we must see second the least profitable order
        context.assert_pop_order(&worst_order);
        // out of orders
        context.assert_pop_none();
    }

    #[test]
    /// Tests 3 tx from the same account, only 1 can succeeded
    fn test_block_orders_competing_orders() {
        let (nonce, mut context) = TestContext::new_1_account(0);
        let middle_order = context.create_add_tx_order(&nonce, 3);
        let best_order = context.create_add_tx_order(&nonce, 5);
        let _worst_order = context.create_add_tx_order(&nonce, 1);
        // we must see first the most profitable order
        context.assert_pop_order(&best_order);
        // we simulate that best_order failed to execute so we don't call update_onchain_nonces
        context.assert_pop_order(&middle_order);
        // we simulate that middle_order excuted
        context.update_nonce(&nonce, 1);
        // we must see none and NOT _worst_order (invalid nonce)
        context.assert_pop_none();
    }

    #[test]
    /// Tests 4 tx from the same account with different nonces.
    fn test_block_orders_pending_orders() {
        let (nonce, mut context) = TestContext::new_1_account(0);
        let first_nonce_order = context.create_add_tx_order(&nonce, 3);
        let second_nonce_order_worst = context.create_add_tx_order(&nonce.clone().with_nonce(1), 5);
        let second_nonce_order_best = context.create_add_tx_order(&nonce.clone().with_nonce(1), 6);
        let _third_nonce_order = context.create_add_tx_order(&nonce.clone().with_nonce(2), 7);

        context.assert_pop_order(&first_nonce_order);
        // Until we update the execution we must see none
        context.assert_pop_none();
        // executed
        context.update_nonce(&nonce, 1);
        context.assert_pop_order(&second_nonce_order_best);
        // second_nonce_order_best failed so we don't update_nonce
        context.assert_pop_order(&second_nonce_order_worst);
        // No more orders for second nonce -> we must see none
        context.assert_pop_none();
        // sim that last tx increased nonce twice so we skipped third_nonce_order_best
        context.update_nonce(&nonce, 3);
        // _third_nonce_order_best was skipped -> none
        context.assert_pop_none();
    }

    #[test]
    // Execute a bundle with an optional tx that fails for invalid nonce
    fn test_block_orders_optional_nonce() {
        let (nonce_1, nonce_2, mut context) = TestContext::new_2_accounts(0, 0);
        let bundle_order =
            context.create_add_bundle_order_2_txs(&nonce_1, true, &nonce_2, false, 1);
        let tx_order = context.create_add_tx_order(&nonce_1, 2);

        // tx_order gives more profit
        context.assert_pop_order(&tx_order);
        // tx_order executed, now tx_order nonce_1 updates
        context.update_nonce(&nonce_1, 1);
        // Even with the first tx failing because of nonce_1 the bundle should be valid
        context.assert_pop_order(&bundle_order);
        // No more orders
        context.assert_pop_none();
    }
}
