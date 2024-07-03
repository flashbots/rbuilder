use tracing::info;

use crate::primitives::{Order, OrderReplacementKey};
use core::fmt::Debug;

/// Receiver of order commands in a low level order stream (mempool + RPC calls)
/// Orders are assumed to be immutable so there is no update.
/// insert_order/remove_order return a bool indicating if the operation was successful.
/// This bool allows the source to cancel notifications on errors if needed.
pub trait ReplaceableOrderSink: Debug + Send {
    fn insert_order(&mut self, order: Order) -> bool;
    fn remove_bundle(&mut self, key: OrderReplacementKey) -> bool;
    /// @Pending remove this ugly hack to check if we can stop sending data.
    /// It should be replaced for a better control over object destruction
    fn is_alive(&self) -> bool;
}

/// Just printlns everything
#[derive(Debug)]
pub struct ReplaceableOrderPrinter {}

impl ReplaceableOrderSink for ReplaceableOrderPrinter {
    fn insert_order(&mut self, order: Order) -> bool {
        info!(
            order_id = ?order.id(),
            order_rep_info = ?order.replacement_key_and_sequence_number(),
            "New order "
        );
        true
    }

    fn remove_bundle(&mut self, key: OrderReplacementKey) -> bool {
        info!(key=?key,"Cancelled  bundle");
        true
    }

    fn is_alive(&self) -> bool {
        true
    }
}

impl Drop for ReplaceableOrderPrinter {
    fn drop(&mut self) {
        println!("OrderPrinter Dropped");
    }
}
