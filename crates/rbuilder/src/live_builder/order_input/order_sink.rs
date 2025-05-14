use mockall::automock;
use tokio::sync::mpsc;
use tracing::info;

use crate::primitives::{Order, OrderId};
use core::fmt::Debug;

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
