use tracing::info;

use core::fmt::Debug;
use rbuilder_primitives::{BundleReplacementData, Order, ShareBundleReplacementKey};

/// Receiver of order commands in a low level order stream (mempool + RPC calls).
/// Orders are assumed to be immutable so there is no update.
/// insert_order/remove_order return a bool indicating if the operation was successful.
/// This bool allows the source to cancel notifications on errors if needed.
/// Some Orders contain replacement_key so they can replace previous ones.
/// Due to source problems insert_order/remove_bundle can arrive out of order so Orders also have a sequence number
/// so we can identify the newest.
pub trait ReplaceableOrderSink: Debug + Send {
    fn insert_order(&mut self, order: Order) -> bool;
    fn remove_bundle(&mut self, replacement_data: BundleReplacementData) -> bool;
    fn remove_sbundle(&mut self, key: ShareBundleReplacementKey) -> bool;
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

    fn remove_bundle(&mut self, replacement_data: BundleReplacementData) -> bool {
        info!(replacement_data=?replacement_data,"Cancelled Bundle");
        true
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn remove_sbundle(&mut self, key: ShareBundleReplacementKey) -> bool {
        info!(key=?key,"Cancelled SBundle");
        true
    }
}

impl Drop for ReplaceableOrderPrinter {
    fn drop(&mut self) {
        println!("OrderPrinter Dropped");
    }
}

#[derive(Debug)]
pub struct NullReplaceableOrderSink {}

impl ReplaceableOrderSink for NullReplaceableOrderSink {
    fn insert_order(&mut self, _order: Order) -> bool {
        true
    }

    fn remove_bundle(&mut self, _replacement_data: BundleReplacementData) -> bool {
        true
    }

    fn remove_sbundle(&mut self, _key: ShareBundleReplacementKey) -> bool {
        true
    }

    fn is_alive(&self) -> bool {
        true
    }
}
