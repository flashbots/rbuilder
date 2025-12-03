use super::{
    AccountNonce, Bundle, BundleReplacementData, BundledTxInfo, MempoolTx, Order, ShareBundle,
    ShareBundleBody, ShareBundleInner, ShareBundleReplacementData, ShareBundleTx,
    TransactionSignedEcRecoveredWithBlobs, TxRevertBehavior, LAST_BUNDLE_VERSION,
};
use alloy_consensus::TxLegacy;
use alloy_primitives::{Address, BlockHash, Signature, TxHash, B256, U256};
use reth_primitives::{Recovered, Transaction, TransactionSigned};
use uuid::Uuid;

/// TestDataGenerator allows you to create unique test objects with unique content, it tries to use different numbers for every field it sets since it may help debugging.
/// The idea is that each module creates its own TestDataGenerator that creates specific data needed in each context.
/// Ideally all other TestDataGenerators will contain one instance of this one for the basic stuff, ideally if several TestDataGenerators are combined the should share this TestDataGenerator
/// to guaranty no repeated data.
/// ALL generated data is based on a single u64 that increments on every use.
/// @Pending factorize with crates/rbuilder/src/mev_boost/rpc.rs (right now we are working on bidding so may generate conflicts)
#[derive(Default)]
pub struct TestDataGenerator {
    last_used_id: u64,
}

impl TestDataGenerator {
    pub fn create_u64(&mut self) -> u64 {
        self.last_used_id += 1;
        self.last_used_id
    }

    pub fn create_u256(&mut self) -> U256 {
        U256::from(self.create_u64())
    }

    pub fn create_uuid(&mut self) -> Uuid {
        Uuid::from_u128(self.create_u128())
    }

    pub fn create_u128(&mut self) -> u128 {
        self.create_u64() as u128
    }

    pub fn create_u8(&mut self) -> u8 {
        self.create_u64() as u8
    }

    pub fn create_address(&mut self) -> Address {
        Address::repeat_byte(self.create_u8())
    }

    pub fn create_block_hash(&mut self) -> BlockHash {
        BlockHash::from(self.create_u256())
    }

    pub fn create_tx_hash(&mut self) -> TxHash {
        TxHash::from(self.create_u256())
    }

    pub fn create_tx(&mut self) -> Recovered<TransactionSigned> {
        self.create_tx_nonce(AccountNonce::default())
    }

    pub fn create_tx_nonce(&mut self, sender_nonce: AccountNonce) -> Recovered<TransactionSigned> {
        Recovered::new_unchecked(
            TransactionSigned::new_unchecked(
                Transaction::Legacy(TxLegacy {
                    nonce: sender_nonce.nonce,
                    ..TxLegacy::default()
                }),
                Signature::test_signature(),
                self.create_tx_hash(),
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
            signer: replacement_data.as_ref().and_then(|r| r.key.key().signer),
            refund_identity: None,
            metadata: Default::default(),
            dropping_tx_hashes: vec![],
            refund: None,
            version: LAST_BUNDLE_VERSION,
            external_hash: None,
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
        ShareBundle::new(
            block,
            block,
            inner_bundle,
            replacement_data.as_ref().and_then(|r| r.key.key().signer),
            replacement_data,
            Vec::new(),
            Default::default(),
        )
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
            signer: replacement_data.as_ref().and_then(|r| r.key.key().signer),
            refund_identity: None,
            metadata: Default::default(),
            dropping_tx_hashes: Default::default(),
            refund: None,
            version: LAST_BUNDLE_VERSION,
            external_hash: None,
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
