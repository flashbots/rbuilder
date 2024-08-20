use ahash::HashMap;

use crate::primitives::{Order, OrderId, OrderReplacementKey};

use super::{order_sink::OrderSink, replaceable_order_sink::ReplaceableOrderSink};

/// Handles all replacement and cancellation for bundles and sbundles by receiving
/// low level orderflow data via ReplaceableOrderSink and forwarding to an OrderSink.
/// The OrderReplacementManager works for a single block.
/// IMPORTANT: Due to infra problems we can get notifications our of order, we must always honor the one
/// with higher sequence_number or the cancel.
/// Although all the structs and fields say "bundle" we always reefer to Bundle or ShareBundle
/// For each bundle we keep the current BundleReplacementState
#[derive(Debug)]
pub struct OrderReplacementManager {
    sink: Box<dyn OrderSink>,
    replacement_states: HashMap<OrderReplacementKey, BundleReplacementState>,
}

impl OrderReplacementManager {
    pub fn new(sink: Box<dyn OrderSink>) -> Self {
        Self {
            sink,
            replacement_states: Default::default(),
        }
    }
}

impl ReplaceableOrderSink for OrderReplacementManager {
    fn insert_order(&mut self, order: Order) -> bool {
        if let Some((rep_key, sequence_number)) = order.replacement_key_and_sequence_number() {
            match self.replacement_states.entry(rep_key) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    e.get_mut()
                        .insert_order(order, sequence_number, &mut self.sink)
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    // New element
                    e.insert(BundleReplacementState::Valid(ValidBundleState {
                        sequence_number,
                        order_id: order.id(),
                    }));
                    self.sink.insert_order(order)
                }
            }
        } else {
            self.sink.insert_order(order)
        }
    }

    fn remove_bundle(&mut self, key: OrderReplacementKey) -> bool {
        match self.replacement_states.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                e.get_mut().cancel_order(&mut self.sink)
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                // New cancelled element (usually out of order notification)
                e.insert(BundleReplacementState::Cancelled);
                true
            }
        }
    }

    fn is_alive(&self) -> bool {
        self.sink.is_alive()
    }
}

#[derive(Debug)]
struct ValidBundleState {
    /// Current valid  sequence_number (larges we've seen)
    pub sequence_number: u64,
    /// OrderId that contained sequence_number. If we upgrade to a new order or cancel we must send a "remove" for this one first.
    pub order_id: OrderId,
}

/// Last state we have for a replaceable ShareBundle.
/// It updates itself on new orders.
/// On new seq:
///     Valid upgrades if seq > current.
///     Cancelled ignores.
/// On Cancel always ends in Cancelled.
#[derive(Debug)]
enum BundleReplacementState {
    /// sequence number
    Valid(ValidBundleState),
    Cancelled,
}

impl BundleReplacementState {
    /// returns false if some operation on the sink returned false
    fn insert_order(
        &mut self,
        order: Order,
        sequence_number: u64,
        sink: &mut Box<dyn OrderSink>,
    ) -> bool {
        match self {
            BundleReplacementState::Valid(valid) => {
                //Update only newer
                if sequence_number > valid.sequence_number {
                    let order_id = order.id();
                    let mut ret = sink.remove_order(valid.order_id);
                    if !sink.insert_order(order) {
                        ret = false;
                    }
                    valid.sequence_number = sequence_number;
                    valid.order_id = order_id;
                    ret
                } else {
                    true
                }
            }
            BundleReplacementState::Cancelled => true, //cancelled -> no more updates
        }
    }

    /// returns false if some operation on the sink returned false
    fn cancel_order(&mut self, sink: &mut Box<dyn OrderSink>) -> bool {
        match self {
            BundleReplacementState::Valid(valid) => {
                let ret = sink.remove_order(valid.order_id);
                *self = BundleReplacementState::Cancelled;
                ret
            }
            BundleReplacementState::Cancelled => true, //cancelled -> no more updates
        }
    }
}

#[cfg(test)]
mod test {
    //use super::*;

    use mockall::predicate::eq;
    use uuid::Uuid;

    use crate::{
        live_builder::order_input::{
            order_sink::MockOrderSink, replaceable_order_sink::ReplaceableOrderSink,
        },
        primitives::{
            AccountNonce, Bundle, BundleReplacementData, BundleReplacementKey, Order,
            OrderReplacementKey, ShareBundle, ShareBundleReplacementData,
            ShareBundleReplacementKey,
        },
    };

    use super::OrderReplacementManager;

    struct TestDataGenerator {
        base: crate::primitives::TestDataGenerator,
        dont_care_nonce: AccountNonce,
    }

    const DONT_CARE_BLOCK: u64 = 0;

    impl TestDataGenerator {
        fn new() -> Self {
            let mut base = crate::primitives::TestDataGenerator::default();
            Self {
                dont_care_nonce: AccountNonce {
                    nonce: 0,
                    account: base.base.create_address(),
                },
                base,
            }
        }

        fn create_bundle(&mut self, replacement_data: Option<BundleReplacementData>) -> Bundle {
            self.base.create_bundle(
                DONT_CARE_BLOCK,
                self.dont_care_nonce.clone(),
                replacement_data,
            )
        }

        fn create_sbundle(
            &mut self,
            replacement_data: Option<ShareBundleReplacementData>,
        ) -> ShareBundle {
            self.base.create_sbundle(
                DONT_CARE_BLOCK,
                self.dont_care_nonce.clone(),
                replacement_data,
            )
        }

        fn create_bundle_replacement_data(&mut self) -> BundleReplacementData {
            BundleReplacementData {
                key: BundleReplacementKey::new(Uuid::new_v4(), self.base.base.create_address()),
                sequence_number: 0,
            }
        }
    }

    /// non_replaceable should pass
    #[test]
    fn test_non_replaceable() {
        let mut data_get = TestDataGenerator::new();
        let bundle = Order::Bundle(data_get.create_bundle(None));
        let bundle_id = bundle.id();
        let mut order_sink = MockOrderSink::new();
        // expect same id forwarded
        order_sink
            .expect_insert_order()
            .times(1)
            .withf(move |o| o.id() == bundle_id)
            .return_const(true);
        let mut manager = OrderReplacementManager::new(Box::new(order_sink));
        manager.insert_order(bundle);
    }

    /// simple insert followed by a cancellation of the order
    #[test]
    fn test_insert_cancel() {
        let mut data_gen = TestDataGenerator::new();
        let replacement_data = data_gen.create_bundle_replacement_data();
        let bundle = Order::Bundle(data_gen.create_bundle(Some(replacement_data.clone())));
        let mut order_sink = MockOrderSink::new();

        // expect order added
        let bundle_id = bundle.id();
        order_sink
            .expect_insert_order()
            .times(1)
            .withf(move |o| o.id() == bundle_id)
            .return_const(true);

        // expect order removed
        let bundle_id = bundle.id();
        order_sink
            .expect_remove_order()
            .times(1)
            .with(eq(bundle_id))
            .return_const(true);

        let mut manager = OrderReplacementManager::new(Box::new(order_sink));
        manager.insert_order(bundle);
        manager.remove_bundle(OrderReplacementKey::Bundle(replacement_data.key));
    }

    /// cancel should not notify anything
    #[test]
    fn test_cancel() {
        let mut data_gen = TestDataGenerator::new();
        let replacement_data = data_gen.create_bundle_replacement_data();
        let order_sink = MockOrderSink::new();
        let mut manager = OrderReplacementManager::new(Box::new(order_sink));
        manager.remove_bundle(OrderReplacementKey::Bundle(replacement_data.key));
    }

    /// cancel before insert should not notify anything
    #[test]
    fn test_cancel_insert() {
        let mut data_gen = TestDataGenerator::new();
        let replacement_data = data_gen.create_bundle_replacement_data();
        let bundle = Order::Bundle(data_gen.create_bundle(Some(replacement_data.clone())));
        let order_sink = MockOrderSink::new();

        let mut manager = OrderReplacementManager::new(Box::new(order_sink));
        manager.remove_bundle(OrderReplacementKey::Bundle(replacement_data.key));
        manager.insert_order(bundle);
    }

    /// replacement with sequence increase should show both versions.
    #[test]
    fn test_increase_seq() {
        let mut data_gen = TestDataGenerator::new();
        let old_replacement_data = data_gen.create_bundle_replacement_data();
        let new_replacement_data = old_replacement_data.next();
        let old_bundle = Order::Bundle(data_gen.create_bundle(Some(old_replacement_data.clone())));
        let new_bundle = Order::Bundle(data_gen.create_bundle(Some(new_replacement_data)));

        let mut order_sink = MockOrderSink::new();

        // expect order added
        let old_bundle_id = old_bundle.id();
        order_sink
            .expect_insert_order()
            .times(1)
            .withf(move |o| o.id() == old_bundle_id)
            .return_const(true);

        // expect order removed
        let old_bundle_id = old_bundle.id();
        order_sink
            .expect_remove_order()
            .times(1)
            .with(eq(old_bundle_id))
            .return_const(true);

        // expect new version added
        let new_bundle_id = new_bundle.id();
        order_sink
            .expect_insert_order()
            .times(1)
            .withf(move |o| o.id() == new_bundle_id)
            .return_const(true);

        let mut manager = OrderReplacementManager::new(Box::new(order_sink));
        manager.insert_order(old_bundle);
        manager.insert_order(new_bundle);
    }

    /// replacement with sequence decrease should ignore the older version.
    #[test]
    fn test_decrease_seq() {
        let mut data_gen = TestDataGenerator::new();
        let old_replacement_data = data_gen.create_bundle_replacement_data();
        let new_replacement_data = old_replacement_data.next();
        let old_bundle = Order::Bundle(data_gen.create_bundle(Some(old_replacement_data.clone())));
        let new_bundle = Order::Bundle(data_gen.create_bundle(Some(new_replacement_data)));

        let mut order_sink = MockOrderSink::new();

        // expect new version added
        let new_bundle_id = new_bundle.id();
        order_sink
            .expect_insert_order()
            .times(1)
            .withf(move |o| o.id() == new_bundle_id)
            .return_const(true);

        let mut manager = OrderReplacementManager::new(Box::new(order_sink));
        manager.insert_order(new_bundle);
        manager.insert_order(old_bundle);
    }

    /// bundle uuids and sbundle uuids should be independent (can repeat and everything should work).
    #[test]
    fn test_bundle_sbundle_mix() {
        let mut data_gen = TestDataGenerator::new();
        let bundle_replacement_data = data_gen.create_bundle_replacement_data();
        let sbundle_replacement_data = ShareBundleReplacementData {
            key: ShareBundleReplacementKey::new(
                bundle_replacement_data.key.key().id,
                bundle_replacement_data.key.key().signer,
            ),
            sequence_number: bundle_replacement_data.sequence_number,
        };
        let bundle = Order::Bundle(data_gen.create_bundle(Some(bundle_replacement_data.clone())));
        let sbundle =
            Order::ShareBundle(data_gen.create_sbundle(Some(sbundle_replacement_data.clone())));

        let mut order_sink = MockOrderSink::new();
        // expect bundle added
        let bundle_id = bundle.id();
        order_sink
            .expect_insert_order()
            .times(1)
            .withf(move |o| o.id() == bundle_id)
            .return_const(true);
        // expect sbundle added
        let sbundle_id = sbundle.id();
        order_sink
            .expect_insert_order()
            .times(1)
            .withf(move |o| o.id() == sbundle_id)
            .return_const(true);
        // expect bundle removed
        let bundle_id = bundle.id();
        order_sink
            .expect_remove_order()
            .times(1)
            .with(eq(bundle_id))
            .return_const(true);
        // expect sbundle removed
        let sbundle_id = sbundle.id();
        order_sink
            .expect_remove_order()
            .times(1)
            .with(eq(sbundle_id))
            .return_const(true);

        let mut manager = OrderReplacementManager::new(Box::new(order_sink));
        manager.insert_order(bundle);
        manager.insert_order(sbundle);
        manager.remove_bundle(OrderReplacementKey::Bundle(bundle_replacement_data.key));
        manager.remove_bundle(OrderReplacementKey::ShareBundle(
            sbundle_replacement_data.key,
        ));
    }
}
