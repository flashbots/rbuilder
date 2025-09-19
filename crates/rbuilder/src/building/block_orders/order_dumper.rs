//! Soon to be replaced my mockall

use std::{collections::VecDeque, sync::Arc};

use rbuilder_primitives::{OrderId, SimulatedOrder};

use super::SimulatedOrderSink;
pub enum OrderStoreAction {
    Insert(Arc<SimulatedOrder>),
    Remove(OrderId),
}

/// Helper to analyze the generated flow of orders
/// The idea es to create this as a sink for a source object and the
/// when we execute something on the source we check via pop_insert, pop_remove,etc
/// if the behavior was correct.
#[derive(Default)]
pub struct OrderDumper {
    pub actions: VecDeque<OrderStoreAction>,
}

impl SimulatedOrderSink for OrderDumper {
    fn insert_order(&mut self, order: Arc<SimulatedOrder>) {
        self.actions.push_back(OrderStoreAction::Insert(order));
    }

    fn remove_order(&mut self, id: OrderId) -> Option<Arc<SimulatedOrder>> {
        self.actions.push_back(OrderStoreAction::Remove(id));
        None
    }
}

impl Drop for OrderDumper {
    fn drop(&mut self) {
        // Every action must be analyzed
        assert!(self.actions.is_empty());
    }
}

impl OrderDumper {
    /// # Panics
    /// empty or first not insert
    pub fn pop_insert(&mut self) -> Arc<SimulatedOrder> {
        if self.actions.is_empty() {
            panic!("No actions, expected insert");
        }
        match self.actions.pop_front().unwrap() {
            OrderStoreAction::Insert(sim_order) => sim_order,
            OrderStoreAction::Remove(_) => panic!("Expected insert found remove"),
        }
    }

    /// # Panics
    /// empty or first not remove
    pub fn pop_remove(&mut self) -> OrderId {
        if self.actions.is_empty() {
            panic!("No actions, expected insert");
        }
        match self.actions.pop_front().unwrap() {
            OrderStoreAction::Insert(_) => panic!("Expected remove found insert"),
            OrderStoreAction::Remove(id) => id,
        }
    }
}
