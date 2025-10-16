use std::mem;

use super::{
    Bundle, BundleRefund, BundleReplacementData, MempoolTx, Order, OrderId, Refund, RefundConfig,
    ShareBundle, ShareBundleBody, ShareBundleInner, ShareBundleTx,
    TransactionSignedEcRecoveredWithBlobs, TxRevertBehavior, LAST_BUNDLE_VERSION,
};

/// Helper object to build Orders for testing.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum OrderBuilder {
    MempoolTx(Option<TransactionSignedEcRecoveredWithBlobs>),
    Bundle(BundleBuilder),
    ShareBundle(ShareBundleBuilder),
    None,
}

impl OrderBuilder {
    pub fn build_order(&mut self) -> Order {
        let builder = mem::replace(self, OrderBuilder::None);
        match builder {
            OrderBuilder::MempoolTx(tx) => {
                let tx = tx.expect("No transactions for mempool tx order.");
                Order::Tx(MempoolTx::new(tx))
            }
            OrderBuilder::Bundle(builder) => Order::Bundle(builder.build()),
            OrderBuilder::ShareBundle(builder) => Order::ShareBundle(builder.build()),
            OrderBuilder::None => panic!("Order building was not started"),
        }
    }

    pub fn assert_none(&self) {
        assert!(
            matches!(self, OrderBuilder::None),
            "Order should be finished before starting a new one"
        )
    }

    pub fn start_bundle_builder(&mut self, block: u64) {
        self.assert_none();
        *self = OrderBuilder::Bundle(BundleBuilder::new(block))
    }

    pub fn start_share_bundle_builder(&mut self, block: u64, max_block: u64) {
        self.assert_none();
        *self = OrderBuilder::ShareBundle(ShareBundleBuilder::new(block, max_block));
    }

    pub fn start_mempool_tx_builder(&mut self) {
        self.assert_none();
        *self = OrderBuilder::MempoolTx(None);
    }

    pub fn add_tx(
        &mut self,
        tx_with_blobs: TransactionSignedEcRecoveredWithBlobs,
        revert_behavior: TxRevertBehavior,
    ) {
        match self {
            OrderBuilder::MempoolTx(opt) => {
                assert!(opt.is_none(), "Only one tx can be inside mempool tx order");
                assert!(
                    revert_behavior.can_revert(),
                    "Mempool txs must be revertable"
                );
                *opt = Some(tx_with_blobs);
            }
            OrderBuilder::Bundle(builder) => {
                builder.add_tx(tx_with_blobs, revert_behavior);
            }
            OrderBuilder::ShareBundle(builder) => {
                builder.add_tx(tx_with_blobs, revert_behavior);
            }
            OrderBuilder::None => {
                panic!("Order building was not started");
            }
        }
    }

    // bundle methods
    pub fn set_bundle_timestamp(&mut self, min_timestamp: Option<u64>, max_timestamp: Option<u64>) {
        match self {
            OrderBuilder::Bundle(builder) => {
                builder.set_bundle_timestamp(min_timestamp, max_timestamp);
            }
            _ => panic!("Only Bundle can have timestamp params"),
        }
    }

    pub fn set_bundle_replacement_data(&mut self, data: BundleReplacementData) {
        match self {
            OrderBuilder::Bundle(builder) => {
                builder.set_bundle_replacement_data(data);
            }
            _ => panic!("Only Bundle can have timestamp params"),
        }
    }

    // nested bundle methods
    pub fn start_inner_bundle(&mut self, can_skip: bool) {
        match self {
            OrderBuilder::ShareBundle(builder) => {
                builder.start_inner_bundle(can_skip);
            }
            _ => panic!("Only ShareBundle can have inner bundle"),
        }
    }

    pub fn finish_inner_bundle(&mut self) {
        match self {
            OrderBuilder::ShareBundle(builder) => {
                builder.finish_inner_bundle();
            }
            _ => panic!("Only ShareBundle can have inner bundle"),
        }
    }

    pub fn set_inner_bundle_refund(&mut self, refund: Vec<Refund>) {
        match self {
            OrderBuilder::ShareBundle(builder) => {
                builder.set_inner_bundle_refund(refund);
            }
            _ => panic!("Only ShareBundle can have refund"),
        }
    }

    pub fn set_bundle_refund(&mut self, refund: BundleRefund) {
        match self {
            OrderBuilder::Bundle(builder) => {
                builder.set_bundle_refund(refund);
            }
            _ => panic!("Only Bundle can have BundleRefund"),
        }
    }

    pub fn set_inner_bundle_refund_config(&mut self, refund_config: Vec<RefundConfig>) {
        match self {
            OrderBuilder::ShareBundle(builder) => {
                builder.set_inner_bundle_refund_config(refund_config);
            }
            _ => panic!("Only ShareBundle can have refund config"),
        }
    }

    pub fn set_inner_bundle_original_order_id(&mut self, original_order_id: OrderId) {
        match self {
            OrderBuilder::ShareBundle(builder) => {
                builder.set_inner_bundle_original_order_id(original_order_id);
            }
            _ => panic!("Only ShareBundle can have refund config"),
        }
    }
}

#[derive(Debug)]
pub struct BundleBuilder {
    block: u64,
    txs: Vec<(TransactionSignedEcRecoveredWithBlobs, TxRevertBehavior)>,
    min_timestamp: Option<u64>,
    max_timestamp: Option<u64>,
    replacement_data: Option<BundleReplacementData>,
    refund: Option<BundleRefund>,
}

impl BundleBuilder {
    fn new(block: u64) -> Self {
        Self {
            block,
            txs: vec![],
            min_timestamp: None,
            max_timestamp: None,
            replacement_data: None,
            refund: None,
        }
    }

    fn set_bundle_timestamp(&mut self, min_timestamp: Option<u64>, max_timestamp: Option<u64>) {
        self.min_timestamp = min_timestamp;
        self.max_timestamp = max_timestamp;
    }

    fn set_bundle_replacement_data(&mut self, data: BundleReplacementData) {
        self.replacement_data = Some(data);
    }

    fn set_bundle_refund(&mut self, refund: BundleRefund) {
        self.refund = Some(refund);
    }

    fn build(self) -> Bundle {
        let mut reverting_tx_hashes = Vec::new();
        let mut dropping_tx_hashes = Vec::new();
        let mut txs = Vec::new();
        for (tx_with_blobs, revert_behavior) in self.txs {
            match revert_behavior {
                TxRevertBehavior::NotAllowed => {}
                TxRevertBehavior::AllowedIncluded => {
                    reverting_tx_hashes.push(*tx_with_blobs.tx.hash())
                }
                TxRevertBehavior::AllowedExcluded => {
                    dropping_tx_hashes.push(*tx_with_blobs.tx.hash())
                }
            }
            txs.push(tx_with_blobs);
        }
        let mut bundle = Bundle {
            block: Some(self.block),
            min_timestamp: self.min_timestamp,
            max_timestamp: self.max_timestamp,
            txs,
            reverting_tx_hashes,
            hash: Default::default(),
            uuid: Default::default(),
            replacement_data: self.replacement_data,
            signer: None,
            refund_identity: None,
            metadata: Default::default(),
            dropping_tx_hashes,
            refund: self.refund,
            version: LAST_BUNDLE_VERSION,
            external_hash: None,
        };
        bundle.hash_slow();
        bundle
    }

    fn add_tx(
        &mut self,
        tx_with_blobs: TransactionSignedEcRecoveredWithBlobs,
        revert_behavior: TxRevertBehavior,
    ) {
        self.txs.push((tx_with_blobs, revert_behavior));
    }
}

#[derive(Debug)]
pub struct ShareBundleBuilder {
    block: u64,
    max_block: u64,
    inner_bundle_stack: Vec<ShareBundleInner>,
}

impl ShareBundleBuilder {
    fn new(block: u64, max_block: u64) -> Self {
        let mut res = Self {
            block,
            max_block,
            inner_bundle_stack: Vec::new(),
        };
        res.start_inner_bundle(false);
        res
    }

    fn build(mut self) -> ShareBundle {
        let inner_bundle = self.inner_bundle_stack.pop().unwrap();
        ShareBundle::new(
            self.block,
            self.max_block,
            inner_bundle,
            None,
            None,
            Vec::new(),
            Default::default(),
        )
    }

    fn start_inner_bundle(&mut self, can_skip: bool) {
        self.inner_bundle_stack.push(ShareBundleInner {
            body: vec![],
            refund: vec![],
            refund_config: vec![],
            can_skip,
            original_order_id: None,
        });
    }

    fn set_inner_bundle_refund(&mut self, refund: Vec<Refund>) {
        let last = self.inner_bundle_stack.last_mut().unwrap();
        last.refund = refund;
    }

    fn set_inner_bundle_refund_config(&mut self, refund_config: Vec<RefundConfig>) {
        let last = self.inner_bundle_stack.last_mut().unwrap();
        last.refund_config = refund_config;
    }

    fn set_inner_bundle_original_order_id(&mut self, original_order_id: OrderId) {
        let last = self.inner_bundle_stack.last_mut().unwrap();
        last.original_order_id = Some(original_order_id);
    }

    fn finish_inner_bundle(&mut self) {
        let inner_bundle = self.inner_bundle_stack.pop().unwrap();
        let last = self.inner_bundle_stack.last_mut().unwrap();
        last.body.push(ShareBundleBody::Bundle(inner_bundle));
    }

    fn add_tx(
        &mut self,
        tx: TransactionSignedEcRecoveredWithBlobs,
        revert_behavior: TxRevertBehavior,
    ) {
        let last = self.inner_bundle_stack.last_mut().unwrap();
        last.body.push(ShareBundleBody::Tx(ShareBundleTx {
            tx,
            revert_behavior,
        }));
    }
}
