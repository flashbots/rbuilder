use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::building::{
    builders::OrderConsumer,
    journal::SimulatedOrderJournalCommand,
    priority_update::{
        pur_simulation_job::{PUResultSubscription, PUSimulationContext},
        PriorityUpdatePool,
    },
    SimulatedOrderStore,
};
use rbuilder_primitives::SimulatedOrder;

const PU_DRAIN_LIMIT: usize = 256;

/// Struct that allow getting the new orders from the order/cancellation stream in the way the parallel builder likes it.
/// Contains the current whole set of orders but also can be queried for deltas on the orders ONLY if the deltas are all additions.
///
/// PU sims arrive on a dedicated subscription to the priority-update result
/// scheduler and are folded into the shared [`PriorityUpdatePool`]; the
/// journal stream only carries regular (non-PU) orders.
///
/// Usage:
/// call consume_next_batch to poll the source and internally store the new orders
/// call drain_new_orders/get_orders
pub struct OrderIntakeStore {
    order_consumer: OrderConsumer,
    order_sink: SimulatedOrderStore,
    priority_update_pool: Arc<RwLock<PriorityUpdatePool>>,
    pu_subscription: PUResultSubscription,
    pu_buf: Vec<(Uuid, u64, Option<Arc<SimulatedOrder>>)>,
}

impl OrderIntakeStore {
    pub fn new(
        orders_input_stream: broadcast::Receiver<SimulatedOrderJournalCommand>,
        pu_context: &PUSimulationContext,
    ) -> Self {
        Self {
            order_consumer: OrderConsumer::new(orders_input_stream),
            order_sink: SimulatedOrderStore::new(),
            priority_update_pool: Arc::new(RwLock::new(PriorityUpdatePool::new())),
            pu_subscription: pu_context.subscribe(),
            pu_buf: Vec::new(),
        }
    }

    pub fn priority_update_pool(&self) -> Arc<RwLock<PriorityUpdatePool>> {
        Arc::clone(&self.priority_update_pool)
    }

    pub fn consume_next_batch(&mut self) -> eyre::Result<bool> {
        self.order_consumer.blocking_consume_next_commands()?;
        self.order_consumer.apply_new_commands(&mut self.order_sink);
        // Drain priority updates so we have the latest priority updates
        self.pu_buf.clear();
        self.pu_subscription
            .pop_unprocessed_events(PU_DRAIN_LIMIT, &mut self.pu_buf);
        if !self.pu_buf.is_empty() {
            let mut pool = self.priority_update_pool.write();
            pool.apply_events(self.pu_buf.drain(..));
        }
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
