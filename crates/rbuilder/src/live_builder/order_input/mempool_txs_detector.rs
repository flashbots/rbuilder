use ahash::RandomState;
use alloy_primitives::TxHash;
use dashmap::DashSet;

use rbuilder_primitives::TransactionSignedEcRecoveredWithBlobs;

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

    pub fn add_tx(&self, hash: TxHash) {
        self.mempool_txs.insert(hash);
    }

    pub fn remove_tx(&self, hash: TxHash) {
        self.mempool_txs.remove(&hash);
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
