use ahash::HashMap;
use mockall::automock;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{info, warn};

use core::fmt::Debug;
use rbuilder_primitives::{Order, OrderId};
use std::sync::Arc;

/// Receiver of order commands.
/// No replacement/cancellation (or version checking) is considered here.
/// Orders are assumed to be immutable so there is no update.
/// insert_order/remove_order return a bool indicating if the operation was successful.
/// This bool allows the source to cancel notifications on errors if needed.
#[automock]
pub trait OrderSink: Debug + Send {
    fn insert_order(&mut self, order: Order) -> bool;
    fn remove_order(&mut self, id: OrderId) -> bool;
    /// @Pending remove this ugly hack to check if we can stop sending data.
    /// It should be replaced for a better control over object destruction
    fn is_alive(&self) -> bool;
}

/// Just printlns everything
#[derive(Debug)]
pub struct OrderPrinter {}

impl OrderSink for OrderPrinter {
    fn insert_order(&mut self, order: Order) -> bool {
        info!(order_id = ?order.id() ,"New order");
        true
    }

    fn remove_order(&mut self, id: OrderId) -> bool {
        info!(order_id = ?id ,"Cancelled order");
        true
    }

    fn is_alive(&self) -> bool {
        true
    }
}

impl Drop for OrderPrinter {
    fn drop(&mut self) {
        println!("OrderPrinter Dropped");
    }
}

///////////////////////

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum OrderPoolCommand {
    //OrderSink::insert_order
    Insert(Order),
    //OrderSink::remove_order
    Remove(OrderId),
}

/// Adapts push Order flow to pull flow.
#[derive(Debug)]
pub struct OrderSender2OrderSink {
    sender: mpsc::UnboundedSender<OrderPoolCommand>,
}

impl OrderSender2OrderSink {
    /// returns the OrderSender2OrderSink to get the flow and the UnboundedReceiver to poll the flow.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<OrderPoolCommand>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self { sender }, receiver)
    }
}

impl OrderSink for OrderSender2OrderSink {
    fn insert_order(&mut self, order: Order) -> bool {
        self.sender.send(OrderPoolCommand::Insert(order)).is_ok()
    }

    fn remove_order(&mut self, id: OrderId) -> bool {
        self.sender.send(OrderPoolCommand::Remove(id)).is_ok()
    }

    fn is_alive(&self) -> bool {
        !self.sender.is_closed()
    }
}

/// Implement OrderSink executing all commands and give the current orders.
/// Usage: create, call OrderSink funcs and call into_orders/orders to get the current orders.
#[derive(Debug)]
pub struct OrderStore {
    orders: HashMap<OrderId, Order>,
}

impl OrderStore {
    pub fn new() -> Self {
        Self {
            orders: Default::default(),
        }
    }

    pub fn into_orders(self) -> Vec<Order> {
        self.orders.into_values().collect()
    }

    pub fn orders(&self) -> Vec<Order> {
        self.orders.values().cloned().collect()
    }
}

impl Default for OrderStore {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderSink for OrderStore {
    fn insert_order(&mut self, order: Order) -> bool {
        if let Some(old_order) = self.orders.insert(order.id(), order) {
            warn!(id =?old_order.id(), "Replacing an already inserted order");
        }
        true
    }

    fn remove_order(&mut self, id: OrderId) -> bool {
        if self.orders.remove(&id).is_none() {
            warn!(id =?id, "Order to remove not found");
        }
        true
    }

    fn is_alive(&self) -> bool {
        true
    }
}

/// Allows to share an OrderSink (adds a mutex) since sometimes you need to give away ownership on an Box
#[derive(Debug)]
pub struct ShareableOrderSink<OrderSinkType> {
    pub sink: Arc<Mutex<OrderSinkType>>,
}

impl<OrderSinkType: OrderSink> ShareableOrderSink<OrderSinkType> {
    pub fn new(sink: Arc<Mutex<OrderSinkType>>) -> Self {
        Self { sink }
    }
}

impl<OrderSinkType: OrderSink> OrderSink for ShareableOrderSink<OrderSinkType> {
    fn insert_order(&mut self, order: Order) -> bool {
        self.sink.lock().insert_order(order)
    }

    fn remove_order(&mut self, id: OrderId) -> bool {
        self.sink.lock().remove_order(id)
    }

    fn is_alive(&self) -> bool {
        self.sink.lock().is_alive()
    }
}
