use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::broadcast;

use crate::building::{
    builders::OrderConsumer, journal::SimulatedOrderJournalCommand,
    priority_update::PriorityUpdatePool, SimulatedOrderSink, SimulatedOrderStore,
};
use rbuilder_primitives::{OrderId, SimulatedOrder};

/// Struct that allow getting the new orders from the order/cancellation stream in the way the parallel builder likes it.
/// Contains the current whole set of orders but also can be queried for deltas on the orders ONLY if the deltas are all additions
///
/// Orders carrying [`SimulatedOrder::pu_data`] are routed to a shared
/// [`PriorityUpdatePool`] and never enter the regular order set; everything
/// else flows through `order_sink`.
///
/// Usage:
/// call consume_next_batch to poll the source and internally store the new orders
/// call drain_new_orders/get_orders
pub struct OrderIntakeStore {
    order_consumer: OrderConsumer,
    order_sink: SimulatedOrderStore,
    priority_update_pool: Arc<RwLock<PriorityUpdatePool>>,
}

impl OrderIntakeStore {
    pub fn new(orders_input_stream: broadcast::Receiver<SimulatedOrderJournalCommand>) -> Self {
        Self {
            order_consumer: OrderConsumer::new(orders_input_stream),
            order_sink: SimulatedOrderStore::new(),
            priority_update_pool: Arc::new(RwLock::new(PriorityUpdatePool::new())),
        }
    }

    pub fn priority_update_pool(&self) -> Arc<RwLock<PriorityUpdatePool>> {
        Arc::clone(&self.priority_update_pool)
    }

    pub fn consume_next_batch(&mut self) -> eyre::Result<bool> {
        self.order_consumer.blocking_consume_next_commands()?;
        let mut pool = self.priority_update_pool.write();
        let mut sink = RoutingSink {
            store: &mut self.order_sink,
            pool: &mut pool,
        };
        self.order_consumer.apply_new_commands(&mut sink);
        Ok(true)
    }

    /// returns the new orders since last call if we ONLY had new orders (no cancellations allowed)
    pub fn try_drain_new_orders_if_no_cancellations(&mut self) -> Option<Vec<Arc<SimulatedOrder>>> {
        self.order_sink.drain_new_orders()
    }

    /// All the current non-PU orders
    pub fn get_orders(&self) -> Vec<Arc<SimulatedOrder>> {
        self.order_sink.get_orders()
    }
}

/// Sink that routes simulated orders by their PU status: PU orders into the
/// pool, everything else into the regular store.
struct RoutingSink<'a> {
    store: &'a mut SimulatedOrderStore,
    pool: &'a mut PriorityUpdatePool,
}

impl SimulatedOrderSink for RoutingSink<'_> {
    fn insert_order(&mut self, order: Arc<SimulatedOrder>) {
        if order.pu_data.is_some() {
            self.pool.apply_update(order);
        } else {
            self.store.insert_order(order);
        }
    }

    fn remove_order(&mut self, id: OrderId) -> Option<Arc<SimulatedOrder>> {
        self.pool.apply_remove(&id);
        self.store.remove_order(id)
    }
}
