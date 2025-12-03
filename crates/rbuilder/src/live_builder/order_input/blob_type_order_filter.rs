use alloy_eips::{eip7594::BlobTransactionSidecarVariant, Typed2718};

use crate::live_builder::order_input::replaceable_order_sink::ReplaceableOrderSink;
use rbuilder_primitives::{
    BundleReplacementData, Order, ShareBundleReplacementKey, TransactionSignedEcRecoveredWithBlobs,
};

/// Filters out Orders with incorrect blobs (pre/post fusaka).
/// Since it's very unlikely what we have many wrong blobs we only filter on insert_order without take note of filtered orders.
/// If remove_bundle/remove_sbundle is called we just forward the call to the sink so it might try to remove a filtered order.
pub struct BlobTypeOrderFilter<FilterFunc> {
    sink: Box<dyn ReplaceableOrderSink>,
    ///true if it likes the blob sidecar, false if it doesn't (Order gets filtered).
    filter_func: FilterFunc,
}

impl<FilterFunc> std::fmt::Debug for BlobTypeOrderFilter<FilterFunc> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobTypeOrderFilter")
            .field("sink", &"<dyn ReplaceableOrderSink>")
            .finish()
    }
}

/// Filters out EIP-7594 style blobs, supports only EIP-4844 style.
pub fn new_pre_fusaka(
    sink: Box<dyn ReplaceableOrderSink>,
) -> BlobTypeOrderFilter<impl Fn(&TransactionSignedEcRecoveredWithBlobs) -> bool + Send + Sync> {
    BlobTypeOrderFilter::new(sink, |tx| {
        if tx.is_eip4844() {
            matches!(*tx.blobs_sidecar, BlobTransactionSidecarVariant::Eip4844(_))
        } else {
            true
        }
    })
}

/// Filters out EIP-4844 style, supports only EIP-7594 style blobs.
pub fn new_fusaka(
    sink: Box<dyn ReplaceableOrderSink>,
) -> BlobTypeOrderFilter<impl Fn(&TransactionSignedEcRecoveredWithBlobs) -> bool + Send + Sync> {
    BlobTypeOrderFilter::new(sink, |tx| {
        if tx.is_eip4844() {
            matches!(*tx.blobs_sidecar, BlobTransactionSidecarVariant::Eip7594(_))
        } else {
            true
        }
    })
}

impl<FilterFunc: Fn(&TransactionSignedEcRecoveredWithBlobs) -> bool>
    BlobTypeOrderFilter<FilterFunc>
{
    fn new(sink: Box<dyn ReplaceableOrderSink>, filter_func: FilterFunc) -> Self {
        Self { sink, filter_func }
    }
}

impl<FilterFunc: Fn(&TransactionSignedEcRecoveredWithBlobs) -> bool + Send + Sync>
    ReplaceableOrderSink for BlobTypeOrderFilter<FilterFunc>
{
    fn insert_order(&mut self, order: Order) -> bool {
        if order
            .list_txs()
            .iter()
            .all(|(tx, _)| (self.filter_func)(tx))
        {
            self.sink.insert_order(order)
        } else {
            true
        }
    }

    fn remove_bundle(&mut self, replacement_data: BundleReplacementData) -> bool {
        self.sink.remove_bundle(replacement_data)
    }

    fn remove_sbundle(&mut self, key: ShareBundleReplacementKey) -> bool {
        self.sink.remove_sbundle(key)
    }

    fn is_alive(&self) -> bool {
        self.sink.is_alive()
    }
}
