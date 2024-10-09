use std::{cell::RefCell, rc::Rc};

use alloy_primitives::Address;
use tokio::sync::broadcast;

use crate::{
    building::{
        builders::OrderConsumer, multi_share_bundle_merger::MultiShareBundleMerger,
        SimulatedOrderStore,
    },
    live_builder::simulation::SimulatedOrderCommand,
    primitives::SimulatedOrder,
};

/// Struct that allow getting the new orders from the order/cancellation stream in the way the merging builder likes it.
/// Contains the current whole set of orders but also can be queried for deltas on the orders ONLY if the deltas are all additions
/// Chains MultiShareBundleMerger->SimulatedOrderStore
/// Usage:
/// call consume_next_batch to poll the source and internally store the new orders
/// call drain_new_orders/get_orders
pub struct OrderIntakeStore {
    order_consumer: OrderConsumer,
    share_bundle_merger: Box<MultiShareBundleMerger<SimulatedOrderStore>>,
    order_sink: Rc<RefCell<SimulatedOrderStore>>,
}

impl OrderIntakeStore {
    pub fn new(
        orders_input_stream: broadcast::Receiver<SimulatedOrderCommand>,
        sbundle_merger_selected_signers: &[Address],
    ) -> Self {
        let order_sink = Rc::new(RefCell::new(SimulatedOrderStore::new()));
        let share_bundle_merger = Box::new(MultiShareBundleMerger::new(
            sbundle_merger_selected_signers,
            order_sink.clone(),
        ));

        Self {
            order_consumer: OrderConsumer::new(orders_input_stream),
            share_bundle_merger,
            order_sink,
        }
    }

    pub fn consume_next_batch(&mut self) -> eyre::Result<bool> {
        self.order_consumer.consume_next_commands()?;
        let input: &mut MultiShareBundleMerger<SimulatedOrderStore> = &mut self.share_bundle_merger;
        self.order_consumer.apply_new_commands(input);
        Ok(true)
    }

    /// returns the new orders since last call if we ONLY had new orders (no cancellations allowed)
    pub fn drain_new_orders(&mut self) -> Option<Vec<SimulatedOrder>> {
        (*self.order_sink).borrow_mut().drain_new_orders()
    }

    /// All the current orders
    pub fn get_orders(&self) -> Vec<SimulatedOrder> {
        self.order_sink.borrow().get_orders()
    }
}
