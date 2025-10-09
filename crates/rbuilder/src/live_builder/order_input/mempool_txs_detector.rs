use std::sync::Arc;

use ahash::RandomState;
use alloy_primitives::TxHash;
use dashmap::DashSet;

use rbuilder_primitives::{
    BundleReplacementData, Order, ShareBundleReplacementKey, TransactionSignedEcRecoveredWithBlobs,
};

use super::replaceable_order_sink::ReplaceableOrderSink;

/// Get's in the middle of a ReplaceableOrder stream a feeds a MempoolTxsDetector.
#[derive(Debug)]
pub struct ReplaceableOrderStreamSniffer {
    detector: Arc<MempoolTxsDetector>,
    sink: Box<dyn ReplaceableOrderSink>,
}

impl ReplaceableOrderStreamSniffer {
    pub fn new(sink: Box<dyn ReplaceableOrderSink>, detector: Arc<MempoolTxsDetector>) -> Self {
        Self { detector, sink }
    }

    pub fn detector(&self) -> Arc<MempoolTxsDetector> {
        self.detector.clone()
    }
}

impl ReplaceableOrderSink for ReplaceableOrderStreamSniffer {
    fn insert_order(&mut self, order: Order) -> bool {
        self.detector.add_tx(&order);
        self.sink.insert_order(order)
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

/// Given a TransactionSignedEcRecoveredWithBlobs answers if the tx is from the mempool or not.
/// Current implementation is super simple, it just checks the tx hash against a set of hashes.
#[derive(Debug)]
pub struct MempoolTxsDetector {
    mempool_txs: DashSet<TxHash, RandomState>,
}

impl MempoolTxsDetector {
    pub fn new() -> Self {
        Self {
            mempool_txs: Default::default(),
        }
    }

    pub fn add_tx(&self, order: &Order) {
        if let Order::Tx(mempool_tx) = order {
            self.mempool_txs.insert(mempool_tx.tx_with_blobs.hash());
        }
    }

    pub fn is_mempool(&self, tx: &TransactionSignedEcRecoveredWithBlobs) -> bool {
        self.mempool_txs.contains(&tx.hash())
    }
}

impl Default for MempoolTxsDetector {
    fn default() -> Self {
        Self::new()
    }
}
