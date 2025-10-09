use std::{cell::RefCell, rc::Rc, sync::Arc};

use alloy_primitives::{Address, U256};

use crate::utils::int_percentage;
use rbuilder_primitives::{
    AccountNonce, Order, OrderId, Refund, RefundConfig, ShareBundle, ShareBundleBody,
    ShareBundleInner, ShareBundleTx, SimValue, SimulatedOrder, TxRevertBehavior,
};

use super::{order_dumper::OrderDumper, SimulatedOrderSink, TestDataGenerator};

/// Helper with a ShareBundleMerger connected to a OrderDumper so we can analyze the results
/// Usage:
/// - Create orders via funcs like create_multiple_sbundle_tx_br
/// - Call  insert_order/remove_order
/// - Check expected results (pop_insert/pop_remove) in the expected order.
pub struct TestContext<TestedSinkType> {
    pub data_gen: TestDataGenerator,
    pub dumper: Rc<RefCell<OrderDumper>>,
    pub tested_sink: TestedSinkType,
}

const DEFAULT_REFUND: usize = 90;
const DONT_CARE_BLOCK: u64 = 0;
const DONT_CARE_PROFIT: u64 = 1;
pub const DONT_CARE_GAS_PRICE: u64 = 1;
pub const HI_PROFIT: u64 = 10_000_000;
pub const LOW_PROFIT: u64 = 1_000_000;

impl<TestedSinkType: SimulatedOrderSink> TestContext<TestedSinkType> {
    pub fn new<FactoryType: Fn(Rc<RefCell<OrderDumper>>) -> TestedSinkType>(
        tested_sink_factory: FactoryType,
    ) -> Self {
        let data_gen = TestDataGenerator::default();
        let dumper = Rc::new(RefCell::new(OrderDumper::default()));
        let tested_sink = tested_sink_factory(dumper.clone());
        Self {
            data_gen,
            dumper,
            tested_sink,
        }
    }

    pub fn insert_order(&mut self, order: Arc<SimulatedOrder>) {
        self.tested_sink.insert_order(order);
    }

    pub fn remove_order(&mut self, id: OrderId) -> Option<Arc<SimulatedOrder>> {
        self.tested_sink.remove_order(id)
    }

    pub fn pop_insert(&mut self) -> Arc<SimulatedOrder> {
        self.dumper.borrow_mut().pop_insert()
    }

    pub fn pop_remove(&mut self) -> OrderId {
        self.dumper.borrow_mut().pop_remove()
    }

    fn default_refund_recipient() -> Address {
        Address::default()
    }

    /// 100% of the refund goes to Address::default()
    fn default_refund_config() -> Vec<RefundConfig> {
        vec![RefundConfig {
            address: Self::default_refund_recipient(),
            percent: 100,
        }]
    }

    // DEFAULT_REFUND applies to the tx at index 0
    fn default_tx_br_refund() -> Vec<Refund> {
        vec![Refund {
            body_idx: 0,
            percent: DEFAULT_REFUND,
        }]
    }

    fn create_share_bundle_tx(&mut self, revert_behavior: TxRevertBehavior) -> ShareBundleBody {
        let tx = self
            .data_gen
            .base
            .create_tx_with_blobs_nonce(AccountNonce::default());
        let stx = ShareBundleTx {
            tx,
            revert_behavior,
        };
        ShareBundleBody::Tx(stx)
    }

    /// User tx inside an inner bundle.
    fn create_share_bundle_tx_bundle(
        &mut self,
        revert_behavior: TxRevertBehavior,
    ) -> ShareBundleBody {
        let inner = ShareBundleInner {
            body: vec![self.create_share_bundle_tx(revert_behavior)],
            refund: Default::default(),
            refund_config: Default::default(),
            can_skip: false,
            original_order_id: Default::default(),
        };
        ShareBundleBody::Bundle(inner)
    }

    /// creates the multiple typical tx+backrun with refund to tx
    /// tx is the same in all backruns
    pub fn create_multiple_sbundle_tx_br(&mut self, sbundle_count: usize) -> Vec<ShareBundle> {
        let tx = self.create_share_bundle_tx_bundle(TxRevertBehavior::AllowedExcluded);
        let mut res = Vec::new();
        for _ in 0..sbundle_count {
            let body = vec![
                tx.clone(),
                self.create_share_bundle_tx(TxRevertBehavior::NotAllowed),
            ];
            res.push(self.create_sbundle_from_body(
                body,
                Self::default_tx_br_refund(),
                Self::default_refund_config(),
                None,
            ));
        }
        res
    }

    /// returns a pair of orders (hi,low) hi having more kickbacks than low
    pub fn create_hi_low_orders(
        &mut self,
        hi_signer: Option<Address>,
        low_signer: Option<Address>,
    ) -> (Arc<SimulatedOrder>, Arc<SimulatedOrder>) {
        let mut backruns = self.create_multiple_sbundle_tx_br(2);
        backruns[0].signer = hi_signer;
        backruns[1].signer = low_signer;
        let br_hi = self.create_sim_order(
            Order::ShareBundle(backruns[0].clone()),
            HI_PROFIT,
            DONT_CARE_GAS_PRICE,
        );
        let br_low = self.create_sim_order(
            Order::ShareBundle(backruns[1].clone()),
            LOW_PROFIT,
            DONT_CARE_GAS_PRICE,
        );
        (br_hi, br_low)
    }

    /// creates the typical tx+backrun with refund to tx with selected_signer
    pub fn create_sbundle_tx_br(&mut self) -> ShareBundle {
        self.create_multiple_sbundle_tx_br(1)[0].clone()
    }

    /// Creates and order sending default kickbacks
    pub fn create_sim_order(
        &self,
        order: Order,
        coinbase_profit: u64,
        mev_gas_price: u64,
    ) -> Arc<SimulatedOrder> {
        let sim_value =
            SimValue::new_test_no_gas(U256::from(coinbase_profit), U256::from(mev_gas_price))
                .with_kickbacks(vec![(
                    Self::default_refund_recipient(),
                    U256::from(int_percentage(coinbase_profit, DEFAULT_REFUND)),
                )]);
        Arc::new(SimulatedOrder {
            order,
            sim_value,
            used_state_trace: None,
        })
    }

    /// creates a basic bundle with body and refund/refund_config.
    /// It does NOT check that refund[].body_idx are valid (should be < tx_count)
    fn create_sbundle_from_body(
        &mut self,
        body: Vec<ShareBundleBody>,
        refund: Vec<Refund>,
        refund_config: Vec<RefundConfig>,
        signer: Option<Address>,
    ) -> ShareBundle {
        let inner = ShareBundleInner {
            body,
            refund,
            refund_config,
            can_skip: false,
            original_order_id: None,
        };
        ShareBundle::new(
            DONT_CARE_BLOCK,
            DONT_CARE_BLOCK,
            inner,
            signer,
            None,
            Vec::new(),
            Default::default(),
        )
    }

    fn as_inner(item: &ShareBundleBody) -> &ShareBundleInner {
        match item {
            ShareBundleBody::Tx(_) => {
                panic!("Found ShareBundleBody::Tx, expecting ShareBundleBody::Bundle")
            }
            ShareBundleBody::Bundle(inner) => inner,
        }
    }

    fn as_sbundle(item: &Order) -> &ShareBundle {
        match item {
            Order::Bundle(_) => panic!("Order::Bundle expecting ShareBundle"),
            Order::Tx(_) => panic!("Order::Tx expecting ShareBundle"),
            Order::ShareBundle(sb) => sb,
        }
    }

    /// Checks:
    /// - concatenated_sbundle is composed of all the ShareBundleInner of the sbundles in that order and made skippable.
    /// - the concatenated_sbundle has no refunds.
    /// - SimValue of concatenated_order is the same as the first of sbundles (current expected behavior of merging)
    ///
    /// self is not used but simplifies the call since the static function would need the types specified.
    pub fn assert_concatenated_sbundles_ok(
        &self,
        concatenated_order: &Arc<SimulatedOrder>,
        sbundles: &[Arc<SimulatedOrder>],
    ) {
        let concatenated_sbundle = Self::as_sbundle(&concatenated_order.order);
        assert_eq!(
            concatenated_sbundle.inner_bundle().body.len(),
            sbundles.len()
        );
        assert!(concatenated_sbundle.inner_bundle().refund.is_empty());
        assert!(concatenated_sbundle.inner_bundle().refund_config.is_empty());
        let concatenated_sbundle_inners = concatenated_sbundle
            .inner_bundle()
            .body
            .iter()
            .map(Self::as_inner);
        let sbundles_inners = sbundles
            .iter()
            .map(|sb| Self::as_sbundle(&sb.order).inner_bundle());
        concatenated_sbundle_inners
            .zip(sbundles_inners)
            .for_each(|(conc_sbundle, sbundle)| {
                let mut sbundle = sbundle.clone();
                sbundle.can_skip = true;
                sbundle.original_order_id = conc_sbundle.original_order_id;
                assert_eq!(sbundle, *conc_sbundle);
            });
        if !sbundles.is_empty() {
            assert_eq!(concatenated_order.sim_value, sbundles[0].sim_value);
        }
    }

    /// asserts that the given Order when inserted passes as is to the sink.
    pub fn assert_passes_as_is(&mut self, order: Order) {
        let sim_order =
            self.data_gen
                .create_sim_order(order, DONT_CARE_PROFIT, DONT_CARE_GAS_PRICE);
        self.insert_order(sim_order.clone());
        assert!(self.pop_insert() == sim_order);
    }
}
