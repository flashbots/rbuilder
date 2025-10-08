use std::{
    cell::RefCell,
    cmp::Ordering,
    collections::{hash_map::Entry, BTreeMap},
    rc::Rc,
    sync::Arc,
};

use ahash::HashMap;
use alloy_primitives::{TxHash, B256, U256};
use tracing::{error, warn};

use rbuilder_primitives::{
    Order, OrderId, ShareBundle, ShareBundleBody, ShareBundleInner, SimulatedOrder,
};

use super::SimulatedOrderSink;

/// Mergeable mev-share order broken down to it components for easy handling.
/// Contains
#[derive(Debug, Clone)]
struct BrokenDownShareBundle {
    /// sim order containing the ShareBundle as received
    sim_order: Arc<SimulatedOrder>,
    /// hash of the user inner bundle. This is the identity of the user txs
    user_bundle_hash: B256,
    /// extracted from self.sbundle.sim_value.paid_kickbacks
    user_kickback: U256,
}

impl BrokenDownShareBundle {
    /// This should always be Some since we only create BrokenDownShareBundles from ShareBundle
    pub fn sbundle(&self) -> Option<&ShareBundle> {
        if let Order::ShareBundle(sbundle) = &self.sim_order.order {
            return Some(sbundle);
        }
        None
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct BrokenDownShareBundleSortKey {
    pub order_id: OrderId,
    pub user_kickback: U256,
}

impl BrokenDownShareBundleSortKey {
    pub fn new(order: &BrokenDownShareBundle) -> Self {
        Self {
            order_id: order.sim_order.id(),
            user_kickback: order.user_kickback,
        }
    }
}

impl PartialOrd for BrokenDownShareBundleSortKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Order by kickback for MultiBackrunManager.sorted_orders
impl Ord for BrokenDownShareBundleSortKey {
    fn cmp(&self, other: &Self) -> Ordering {
        let res = self.user_kickback.cmp(&other.user_kickback).reverse(); //reverse -> highers first
        if let Ordering::Equal = res {
            return self.order_id.cmp(&other.order_id);
        }
        res
    }
}

/// Handles the relation between a user_tx and all the orders that backruns it
#[derive(Debug)]
struct MultiBackrunManager<SinkType> {
    /// id of the las that we generated with all sorted_orders
    last_multi_order_id: Option<OrderId>,
    user_bundle_hash: B256,
    /// orders sorted by kickback value
    sorted_orders: BTreeMap<BrokenDownShareBundleSortKey, BrokenDownShareBundle>,
    /// OrderId->Kickback to easily check for an order in sorted_orders
    order_kickbacks: HashMap<OrderId, U256>,
    /// we send here the generated orders
    order_sink: Rc<RefCell<SinkType>>,
}

impl<SinkType: SimulatedOrderSink> MultiBackrunManager<SinkType> {
    pub fn new(user_bundle_hash: B256, order_sink: Rc<RefCell<SinkType>>) -> Self {
        Self {
            user_bundle_hash,
            sorted_orders: BTreeMap::default(),
            order_kickbacks: HashMap::default(),
            last_multi_order_id: None,
            order_sink,
        }
    }

    /// This cloning is tricky since we don't want to clone the Rcs we want to clone the real objects so the sink should be replaced.
    pub fn clone_with_sink(&self, order_sink: Rc<RefCell<SinkType>>) -> Self {
        Self {
            order_sink,
            last_multi_order_id: self.last_multi_order_id,
            user_bundle_hash: self.user_bundle_hash,
            sorted_orders: self.sorted_orders.clone(),
            order_kickbacks: self.order_kickbacks.clone(),
        }
    }

    /// Merge all orders in a single ShareBundle containing all other ShareBundle as skippable items.
    /// All other info for the SimulatedOrder is taken from the first ShareBundle.
    fn merge_orders(&self) -> Option<Arc<SimulatedOrder>> {
        if self.sorted_orders.is_empty() {
            return None;
        }
        let highest_payback_order = self.sorted_orders.first_key_value().unwrap().1;
        let highest_payback_order_bundle = highest_payback_order.sbundle();
        let highest_payback_order_bundle = highest_payback_order_bundle?;
        let mut body = Vec::new();
        let mut original_orders = Vec::new();
        for broken_order in self.sorted_orders.values() {
            if let Some(sbundle) = broken_order.sbundle() {
                let mut inner_bundle = sbundle.inner_bundle().clone();
                inner_bundle.can_skip = true;
                inner_bundle.original_order_id = Some(broken_order.sim_order.id());
                body.push(ShareBundleBody::Bundle(inner_bundle));
                original_orders.push(broken_order.sim_order.order.clone());
            }
        }

        let inner_bundle = ShareBundleInner {
            body,
            refund: Vec::new(),
            refund_config: Vec::new(),
            can_skip: false,
            original_order_id: None,
        };
        let sbundle = ShareBundle::new(
            highest_payback_order_bundle.block,
            highest_payback_order_bundle.max_block,
            inner_bundle,
            highest_payback_order_bundle.signer,
            None, //replacement_data get lost since we merge many sbundles
            original_orders,
            // We take parent order submission time
            highest_payback_order.sim_order.order.metadata().clone(),
        );
        Some(Arc::new(SimulatedOrder {
            order: Order::ShareBundle(sbundle),
            sim_value: highest_payback_order.sim_order.sim_value.clone(),
            used_state_trace: highest_payback_order.sim_order.used_state_trace.clone(),
        }))
    }

    /// On changes calls regenerate_multi_order
    pub fn insert_order(&mut self, order: BrokenDownShareBundle) {
        // Evaluate if it's valid to get a new simulation for the same bundle?
        if self.order_kickbacks.contains_key(&order.sim_order.id()) {
            return;
        }
        self.order_kickbacks
            .insert(order.sim_order.id(), order.user_kickback);
        self.sorted_orders
            .insert(BrokenDownShareBundleSortKey::new(&order), order);
        self.regenerate_multi_order();
    }

    /// On changes calls regenerate_multi_order
    fn remove_order(&mut self, id: OrderId) -> Option<Arc<SimulatedOrder>> {
        let user_kickback = self.order_kickbacks.remove(&id).or_else(|| {
            error!(order_id = ?id, "remove_order for not inserted order");
            None
        })?;
        let key = BrokenDownShareBundleSortKey {
            order_id: id,
            user_kickback,
        };
        let order = self.sorted_orders.remove(&key);
        if let Some(order) = order {
            self.regenerate_multi_order();
            return Some(order.sim_order);
        }
        error!(?key, "sorted order not found");
        None
    }

    /// regenerates the virtual multi order and updates downstream
    fn regenerate_multi_order(&mut self) {
        if let Some(last_multi_order_id) = self.last_multi_order_id {
            self.order_sink
                .borrow_mut()
                .remove_order(last_multi_order_id);
        }
        if self.sorted_orders.is_empty() {
            self.last_multi_order_id = None;
            return;
        }
        let merged_order = self.merge_orders();
        if merged_order.is_none() {
            error!(bundle_hash = ?self.user_bundle_hash, "Failed to generate order for user bundle");
            return;
        }
        let merged_order = merged_order.unwrap();
        self.last_multi_order_id = Some(merged_order.id());
        self.order_sink.borrow_mut().insert_order(merged_order);
    }
}

/// ShareBundleMerger will presume that some of the bundles must be merged.
/// We expect 2 types of sbundles:
/// - UserTxs: It contains only top level tx with no refund since there is no backrun.  (see [`ShareBundleMerger::break_down_user_tx_bundle`] for a clearer definition)
/// - Backruns: It contains a sub bundle with the user txs followed by backrunner sub bundles. (see [`ShareBundleMerger::break_down_backrun_bundle`] for a clearer definition)
///   Merging example: SBundle(Bundle(tx)+br1) + SBundle(Bundle(tx)+br2) will become SBundle(Bundle(Bundle(tx)+br1),Bundle(Bundle(tx)+br2)).
///   User Bundle may contain more that one tx (for the case approve + swap).
///   This means what when we insert/remove orders (sbundles) we insert/remove the virtual bundles we generate and not the original orders.
///   We ONLY use this special flow for orders with some particular structure (see [`ShareBundleMerger::break_down_bundle`]).
///   SBundle not complying with that will pass through and use the standard handling.
///   @Pending evaluate if we can avoid to send changes no every order update and "flush" changes all together (since orders are usually processed on batches)
#[derive(Debug)]
pub struct ShareBundleMerger<SinkType> {
    /// user_tx -> MultiBackrunManagers
    multi_backrun_managers: HashMap<TxHash, Rc<RefCell<MultiBackrunManager<SinkType>>>>,

    /// orders included in some MultiBackrunManager orders for easily deletion
    order_id_2_multi_backrun_managers: HashMap<OrderId, Rc<RefCell<MultiBackrunManager<SinkType>>>>,

    /// all active orders are forwarder to order_sink
    order_sink: Rc<RefCell<SinkType>>,
}

impl<SinkType: SimulatedOrderSink> ShareBundleMerger<SinkType> {
    pub fn new(order_sink: Rc<RefCell<SinkType>>) -> Self {
        Self {
            order_sink,
            multi_backrun_managers: HashMap::default(),
            order_id_2_multi_backrun_managers: HashMap::default(),
        }
    }

    /// This cloning is tricky since we don't want to clone the Rcs we want to clone the real objects so the sink should be replaced.
    /// We also have in order_id_2_multi_backrun_managers references to multi_backrun_managers so we must clone manually :(
    pub fn clone_with_sink(&self, order_sink: Rc<RefCell<SinkType>>) -> Self {
        let mut multi_backrun_managers = HashMap::default();
        let mut order_id_2_multi_backrun_managers = HashMap::default();
        for (tx_hash, manager) in &self.multi_backrun_managers {
            multi_backrun_managers.insert(
                *tx_hash,
                Rc::new(RefCell::new(
                    manager.borrow().clone_with_sink(order_sink.clone()),
                )),
            );
            manager.borrow().sorted_orders.iter().for_each(|(key, _)| {
                order_id_2_multi_backrun_managers.insert(key.order_id, manager.clone());
            });
        }

        Self {
            multi_backrun_managers,
            order_id_2_multi_backrun_managers,
            order_sink,
        }
    }

    const USER_BUNDLE_INDEX: usize = 0;
    /// Tries to analyze if the order is a mergeable sbundle by looking at its structure to check if this is a user txs or a backrun sbundle
    /// Only check here is if Signer is in selected_signers
    fn break_down_bundle(&self, order: &Arc<SimulatedOrder>) -> Option<BrokenDownShareBundle> {
        let sbundle = if let Order::ShareBundle(sbundle) = &order.order {
            sbundle
        } else {
            return None;
        };
        let first_item = sbundle.inner_bundle().body.first().or_else(|| {
            error!(?sbundle, "Empty sbundle");
            None
        })?;

        match first_item {
            ShareBundleBody::Tx(_) => Self::break_down_user_tx_bundle(sbundle, order),
            ShareBundleBody::Bundle(_) => Self::break_down_backrun_bundle(sbundle, order),
        }
    }

    /// This should contain only ShareBundleBody::Tx with no refund (but it can contain refund_info)
    fn break_down_user_tx_bundle(
        sbundle: &ShareBundle,
        sim_order: &Arc<SimulatedOrder>,
    ) -> Option<BrokenDownShareBundle> {
        let got_bundles = sbundle
            .inner_bundle()
            .body
            .iter()
            .any(|item| matches!(item, ShareBundleBody::Bundle(_)));
        if got_bundles {
            warn!(hash = ?sbundle.hash,
                "sbundle for user txs should not contain bundles"
            );
            return None;
        }
        if !sbundle.inner_bundle().refund.is_empty() {
            warn!(hash = ?sbundle.hash,
                "sbundle for user txs should not contain refunds"
            );
            return None;
        }
        Some(BrokenDownShareBundle {
            sim_order: sim_order.clone(),
            user_kickback: U256::from(0),
            user_bundle_hash: sbundle.hash,
        })
    }

    /// To be a backrun bundle it must comply:
    /// - Must contain one refund pointing to the user bundle.
    /// - The first (USER_BUNDLE_INDEX) item (the user bundle) should be a sub bundle with no refund.
    fn break_down_backrun_bundle(
        sbundle: &ShareBundle,
        sim_order: &Arc<SimulatedOrder>,
    ) -> Option<BrokenDownShareBundle> {
        let user_bundle_hash = Self::check_and_get_user_bundle_hash_from_backrun(sbundle)?;
        if !Self::check_refunds_from_backrun_ok(sbundle) {
            return None;
        }
        let mut user_kickback = U256::ZERO;
        for (_, kickback) in sim_order.sim_value.paid_kickbacks() {
            user_kickback += kickback;
        }
        Some(BrokenDownShareBundle {
            sim_order: sim_order.clone(),
            user_kickback,
            user_bundle_hash,
        })
    }

    /// Checks also that it contains no refund stuff
    fn check_and_get_user_bundle_hash_from_backrun(sbundle: &ShareBundle) -> Option<B256> {
        let user_bundle = sbundle
            .inner_bundle()
            .body
            .get(Self::USER_BUNDLE_INDEX)
            .or_else(|| {
                warn!(
                    hash = ?sbundle.hash,
                    "sbundle should have at least {} items",
                    Self::USER_BUNDLE_INDEX + 1
                );
                None
            })?;

        if let ShareBundleBody::Bundle(inner_bundle) = user_bundle {
            if !inner_bundle.refund.is_empty() {
                warn!(
                    hash = ?sbundle.hash,
                    "sbundle user bundle should not contain refunds"
                );
                return None;
            }
            Some(inner_bundle.hash_slow())
        } else {
            warn!(
                hash = ?sbundle.hash,
                "sbundle user bundle should be a ShareBundleInner"
            );
            None
        }
    }

    /// - Have a single inner_bundle.refund (user tx) with body_idx == 0
    fn check_refunds_from_backrun_ok(sbundle: &ShareBundle) -> bool {
        if sbundle.inner_bundle().refund.len() != 1 {
            warn!(
                hash = ?sbundle.hash,
                "sbundle should have a single refund but has {}",
                sbundle.inner_bundle().refund.len()
            );
            return false;
        }
        let first_body_idx = sbundle.inner_bundle().refund[0].body_idx;
        if first_body_idx != Self::USER_BUNDLE_INDEX {
            warn!(
                hash = ?sbundle.hash,
                "sbundle refund[0].body_idx should be {} but is {}",
                Self::USER_BUNDLE_INDEX,
                first_body_idx,
            );
            return false;
        }
        true
    }
}

impl<SinkType: SimulatedOrderSink> SimulatedOrderSink for ShareBundleMerger<SinkType> {
    /// if we can manage the SimulatedOrder send it to the MultiBackrunManager
    /// if not just forward downstream
    fn insert_order(&mut self, order: Arc<SimulatedOrder>) {
        if let Some(broken_down_sbundle) = self.break_down_bundle(&order) {
            let handler = self
                .multi_backrun_managers
                .entry(broken_down_sbundle.user_bundle_hash)
                .or_insert(Rc::new(RefCell::new(MultiBackrunManager::new(
                    broken_down_sbundle.user_bundle_hash,
                    self.order_sink.clone(),
                ))));
            handler.borrow_mut().insert_order(broken_down_sbundle);
            self.order_id_2_multi_backrun_managers
                .insert(order.id(), handler.clone());
        } else {
            self.order_sink.borrow_mut().insert_order(order);
        }
    }

    /// give to handler or forward downstream
    fn remove_order(&mut self, id: OrderId) -> Option<Arc<SimulatedOrder>> {
        match self.order_id_2_multi_backrun_managers.entry(id) {
            Entry::Occupied(handler) => handler.get().borrow_mut().remove_order(id),
            Entry::Vacant(_) => self.order_sink.borrow_mut().remove_order(id),
        }
    }
}

#[cfg(test)]
mod test {

    use crate::building::block_orders::{order_dumper::OrderDumper, test_context::TestContext};
    use rbuilder_primitives::{AccountNonce, Order};

    use super::ShareBundleMerger;

    fn new_test_context() -> TestContext<ShareBundleMerger<OrderDumper>> {
        TestContext::new(ShareBundleMerger::new)
    }

    #[test]
    /// Txs and bundles should pass as is
    fn test_non_sbundle() {
        let mut context = new_test_context();

        // TX
        let order = context
            .data_gen
            .base
            .create_tx_order(AccountNonce::default());
        context.assert_passes_as_is(order);

        // Bundle
        let bundle = context
            .data_gen
            .base
            .create_bundle(0, AccountNonce::default(), None);
        context.assert_passes_as_is(Order::Bundle(bundle));
    }

    #[test]
    /// more than a inner_bundle.refund should pass as is
    fn test_2_refunds() {
        let mut context = new_test_context();
        let sbundle = context.create_sbundle_tx_br();
        let mut new_inner_bundle = sbundle.inner_bundle().clone();
        new_inner_bundle
            .refund
            .push(sbundle.inner_bundle().refund[0].clone());
        let sbundle = sbundle.with_inner_bundle(new_inner_bundle);
        context.assert_passes_as_is(Order::ShareBundle(sbundle));
    }

    #[test]
    /// Test tx+br_hi , tx+br_low. adding and removing and checking that we always have the right generated multisbundle
    fn test_hi_low() {
        let mut context = new_test_context();
        let (br_hi, br_low) = context.create_hi_low_orders(None, None);

        // Insert hi expect an order with only br_hi
        context.insert_order(br_hi.clone());
        let generated_order = context.pop_insert();
        context.assert_concatenated_sbundles_ok(&generated_order, &[br_hi.clone()]);

        // Insert low expect a cancellation for prev order and hi+low
        context.insert_order(br_low.clone());
        assert_eq!(context.pop_remove(), generated_order.id());
        let generated_order = context.pop_insert();
        context.assert_concatenated_sbundles_ok(&generated_order, &[br_hi.clone(), br_low.clone()]);

        // Remove hi order expect a cancellation for prev order and low
        context.remove_order(br_hi.id());
        assert_eq!(context.pop_remove(), generated_order.id());
        let generated_order = context.pop_insert();
        context.assert_concatenated_sbundles_ok(&generated_order, &[br_low.clone()]);

        // Remove low order expect a cancellation for prev order and nothing more (shoudn't insert an empty sbundle!)
        context.remove_order(br_low.id());
        assert_eq!(context.pop_remove(), generated_order.id());

        // We expect an order with only br_low
        context.insert_order(br_low.clone());
        let generated_order = context.pop_insert();
        context.assert_concatenated_sbundles_ok(&generated_order, &[br_low.clone()]);

        // Insert hi expect a cancellation for prev order and hi+low
        context.insert_order(br_hi.clone());
        assert_eq!(context.pop_remove(), generated_order.id());
        let generated_order = context.pop_insert();
        context.assert_concatenated_sbundles_ok(&generated_order, &[br_hi.clone(), br_low.clone()]);

        // reinsert hi shouldn't do anything
        context.insert_order(br_hi.clone());
    }
}
