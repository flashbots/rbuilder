use alloy_consensus::TxLegacy;
use alloy_primitives::B256;
use reth_primitives::{Transaction, TransactionSigned, TransactionSignedEcRecovered};
use revm_primitives::PrimitiveSignature;
use uuid::Uuid;

use super::{
    AccountNonce, Bundle, BundleReplacementData, BundledTxInfo, MempoolTx, Order, ShareBundle,
    ShareBundleBody, ShareBundleInner, ShareBundleReplacementData, ShareBundleTx,
    TransactionSignedEcRecoveredWithBlobs, TxRevertBehavior,
};

/// TestDataGenerator for Orders.
/// Generated orders are not intended to be executed since any data that is not on the create_xxx parameters if default (eg:to,value,input)
#[derive(Default)]
pub struct TestDataGenerator {
    pub base: crate::utils::TestDataGenerator,
}

impl TestDataGenerator {
    pub fn create_tx(&mut self) -> TransactionSignedEcRecovered {
        self.create_tx_nonce(AccountNonce::default())
    }

    pub fn create_tx_nonce(&mut self, sender_nonce: AccountNonce) -> TransactionSignedEcRecovered {
        TransactionSignedEcRecovered::new_unchecked(
            TransactionSigned::new(
                Transaction::Legacy(TxLegacy {
                    nonce: sender_nonce.nonce,
                    ..TxLegacy::default()
                }),
                PrimitiveSignature::test_signature(),
                self.base.create_tx_hash(),
            ),
            sender_nonce.account,
        )
    }

    pub fn create_tx_with_blobs_nonce(
        &mut self,
        sender_nonce: AccountNonce,
    ) -> TransactionSignedEcRecoveredWithBlobs {
        TransactionSignedEcRecoveredWithBlobs::new_no_blobs(self.create_tx_nonce(sender_nonce))
            .unwrap()
    }

    /// Creates a bundle with a single TX (non optional)
    pub fn create_bundle(
        &mut self,
        block: u64,
        sender_nonce: AccountNonce,
        replacement_data: Option<BundleReplacementData>,
    ) -> Bundle {
        let mut res = Bundle {
            block: Some(block),
            min_timestamp: None,
            max_timestamp: None,
            txs: vec![self.create_tx_with_blobs_nonce(sender_nonce)],
            reverting_tx_hashes: vec![],
            hash: B256::default(),
            uuid: Uuid::default(),
            replacement_data: replacement_data.clone(),
            signer: replacement_data.map(|r| r.key.key().signer),
            metadata: Default::default(),
            dropping_tx_hashes: vec![],
            refund: None,
        };
        res.hash_slow();
        res
    }

    /// Creates a sbundle with a single TX (non optional)
    /// No refunds, only useful to check for identity
    pub fn create_sbundle(
        &mut self,
        block: u64,
        sender_nonce: AccountNonce,
        replacement_data: Option<ShareBundleReplacementData>,
    ) -> ShareBundle {
        let inner_bundle = ShareBundleInner {
            body: vec![ShareBundleBody::Tx(ShareBundleTx {
                tx: self.create_tx_with_blobs_nonce(sender_nonce),
                revert_behavior: TxRevertBehavior::NotAllowed,
            })],
            refund: Default::default(),
            refund_config: Default::default(),
            can_skip: true,
            original_order_id: None,
        };
        let mut res = ShareBundle {
            hash: Default::default(),
            block,
            max_block: block,
            inner_bundle,
            signer: replacement_data.as_ref().map(|r| r.key.key().signer),
            replacement_data,
            original_orders: Vec::new(),
            metadata: Default::default(),
        };
        res.hash_slow();
        res
    }

    /// Creates a bundle with a multiple txs
    pub fn create_bundle_multi_tx(
        &mut self,
        block: u64,
        txs_info: &[BundledTxInfo],
        replacement_data: Option<BundleReplacementData>,
    ) -> Bundle {
        let mut reverting_tx_hashes = Vec::new();
        let mut txs = Vec::new();
        for tx_info in txs_info {
            let tx1 = self.create_tx_with_blobs_nonce(tx_info.nonce.clone());
            if tx_info.optional {
                reverting_tx_hashes.push(tx1.hash());
            }
            txs.push(tx1);
        }
        let mut bundle = Bundle {
            block: Some(block),
            min_timestamp: None,
            max_timestamp: None,
            txs,
            reverting_tx_hashes,
            hash: B256::default(),
            uuid: Uuid::default(),
            replacement_data: replacement_data.clone(),
            signer: replacement_data.map(|r| r.key.key().signer),
            metadata: Default::default(),
            dropping_tx_hashes: Default::default(),
            refund: None,
        };
        bundle.hash_slow();
        bundle
    }

    pub fn create_mempool_tx(&mut self, sender_nonce: AccountNonce) -> MempoolTx {
        MempoolTx {
            tx_with_blobs: self.create_tx_with_blobs_nonce(sender_nonce),
        }
    }

    pub fn create_tx_order(&mut self, sender_nonce: AccountNonce) -> Order {
        Order::Tx(self.create_mempool_tx(sender_nonce))
    }

    pub fn create_bundle_multi_tx_order(
        &mut self,
        block: u64,
        txs_info: &[BundledTxInfo],
        replacement_data: Option<BundleReplacementData>,
    ) -> Order {
        Order::Bundle(self.create_bundle_multi_tx(block, txs_info, replacement_data))
    }
}
