use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::broadcast;

use crate::building::{
    builders::OrderConsumer,
    journal::SimulatedOrderJournalCommand,
    priority_update::pur_simulation_job::{PUResultSubscription, PUSimulationContext},
    SimulatedOrderStore,
};
use rbuilder_primitives::SimulatedOrder;

/// Struct that allow getting the new orders from the order/cancellation stream in the way the parallel builder likes it.
/// Contains the current whole set of orders but also can be queried for deltas on the orders ONLY if the deltas are all additions.
///
/// Usage:
/// call consume_next_batch to poll the source and internally store the new orders
/// call drain_new_orders/get_orders
pub struct OrderIntakeStore {
    order_consumer: OrderConsumer,
    order_sink: SimulatedOrderStore,
    pu_subscription: Arc<RwLock<PUResultSubscription>>,
}

impl OrderIntakeStore {
    pub fn new(
        orders_input_stream: broadcast::Receiver<SimulatedOrderJournalCommand>,
        pu_context: &PUSimulationContext,
    ) -> Self {
        Self {
            order_consumer: OrderConsumer::new(orders_input_stream),
            order_sink: SimulatedOrderStore::new(),
            pu_subscription: Arc::new(RwLock::new(pu_context.subscribe())),
        }
    }

    pub fn pu_subscription(&self) -> Arc<RwLock<PUResultSubscription>> {
        Arc::clone(&self.pu_subscription)
    }

    pub fn consume_next_batch(&mut self) -> eyre::Result<bool> {
        self.order_consumer.blocking_consume_next_commands()?;
        self.order_consumer.apply_new_commands(&mut self.order_sink);
        self.pu_subscription
            .write()
            .consume_new_updates_from_subscription();
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
