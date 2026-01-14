use ahash::HashMap;

use rbuilder_primitives::{BundleReplacementData, BundleReplacementKey, Order, OrderId};

use super::{order_sink::OrderSink, replaceable_order_sink::ReplaceableOrderSink};

/// Handle all replacement and cancellation for bundles by receiving
/// low level orderflow data via [`ReplaceableOrderSink`] and forwarding to an
/// [`OrderSink`].
///
/// The `OrderReplacementManager` works for a single block.
///
/// IMPORTANT: Due to infra problems, notifications may arrive out of order.
/// Cancels are treated as highest-priority, and after that we must always
/// honor the replacement with highest `sequence_number`.
///
/// Although all the structs and fields say "bundle" we always refer to Bundle
/// or ShareBundle.
///
/// For each bundle we keep the current [`BundleReplacementState`]
#[derive(Debug)]
pub struct OrderReplacementManager {
    sink: Box<dyn OrderSink>,
    replacement_states: HashMap<BundleReplacementKey, BundleReplacementState>,
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

    fn remove_bundle(&mut self, replacement_data: BundleReplacementData) -> bool {
        match self.replacement_states.entry(replacement_data.key) {
            std::collections::hash_map::Entry::Occupied(mut e) => e
                .get_mut()
                .cancel_order(replacement_data.sequence_number, &mut self.sink),
            std::collections::hash_map::Entry::Vacant(e) => {
                // New cancelled element (usually out of order notification)
                e.insert(BundleReplacementState::Cancelled(
                    replacement_data.sequence_number,
                ));
                true
            }
        }
    }

    fn is_alive(&self) -> bool {
        self.sink.is_alive()
    }
}

#[derive(Debug, Copy, Clone)]
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
/// On Cancel always ends in [`Self::Cancelled`].
#[derive(Debug, Copy, Clone)]
enum BundleReplacementState {
    Valid(ValidBundleState),
    /// Sequence number of the cancellation.
    Cancelled(u64),
}

impl BundleReplacementState {
    fn sequence_number(&self) -> u64 {
        match self {
            BundleReplacementState::Valid(valid_bundle_state) => valid_bundle_state.sequence_number,
            BundleReplacementState::Cancelled(sequence_number) => *sequence_number,
        }
    }

    /// returns false if some operation on the sink returned false
    fn insert_order(
        &mut self,
        order: Order,
        sequence_number: u64,
        sink: &mut Box<dyn OrderSink>,
    ) -> bool {
        if sequence_number <= self.sequence_number() {
            return true;
        }
        let mut res = self.send_remove_order_if_needed(sink);
        let order_id = order.id();
        if !sink.insert_order(order) {
            res = false;
        }
        *self = BundleReplacementState::Valid(ValidBundleState {
            sequence_number,
            order_id,
        });
        res
    }

    /// returns false if some operation on the sink returned false
    fn cancel_order(&mut self, sequence_number: u64, sink: &mut Box<dyn OrderSink>) -> bool {
        if sequence_number <= self.sequence_number() {
            return true;
        }
        let res = self.send_remove_order_if_needed(sink);
        *self = BundleReplacementState::Cancelled(sequence_number);
        res
    }

    /// returns false if some operation on the sink returned false
    fn send_remove_order_if_needed(&self, sink: &mut Box<dyn OrderSink>) -> bool {
        match self {
            BundleReplacementState::Valid(valid) => sink.remove_order(valid.order_id),
            BundleReplacementState::Cancelled(_) => true,
        }
    }
}

#[cfg(test)]
mod test {
    //use super::*;

    use mockall::predicate::eq;
    use uuid::Uuid;

    use crate::live_builder::order_input::{
        order_sink::MockOrderSink, replaceable_order_sink::ReplaceableOrderSink,
    };
    use rbuilder_primitives::{
        AccountNonce, Bundle, BundleReplacementData, BundleReplacementKey, Order,
    };

    use super::OrderReplacementManager;

    struct TestDataGenerator {
        base: rbuilder_primitives::TestDataGenerator,
        dont_care_nonce: AccountNonce,
    }

    const DONT_CARE_BLOCK: u64 = 0;

    impl TestDataGenerator {
        fn new() -> Self {
            let mut base = rbuilder_primitives::TestDataGenerator::default();
            Self {
                dont_care_nonce: AccountNonce {
                    nonce: 0,
                    account: base.create_address(),
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

        fn create_bundle_replacement_data(&mut self) -> BundleReplacementData {
            BundleReplacementData {
                key: BundleReplacementKey::new(Uuid::new_v4(), Some(self.base.create_address())),
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
        let bundle = Order::Bundle(data_gen.create_bundle(Some(replacement_data)));
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
        let cancel_bundle_replacement_data = BundleReplacementData {
            key: replacement_data.key,
            sequence_number: replacement_data.sequence_number + 1,
        };
        manager.remove_bundle(cancel_bundle_replacement_data);
    }

    /// simple insert followed by a cancellation with an old sequence number (should be ignored)
    #[test]
    fn test_insert_ignored_cancel() {
        let mut data_gen = TestDataGenerator::new();
        let replacement_data = data_gen.create_bundle_replacement_data();
        let bundle = Order::Bundle(data_gen.create_bundle(Some(replacement_data)));
        let mut order_sink = MockOrderSink::new();

        // expect order added
        let bundle_id = bundle.id();
        order_sink
            .expect_insert_order()
            .times(1)
            .withf(move |o| o.id() == bundle_id)
            .return_const(true);

        let mut manager = OrderReplacementManager::new(Box::new(order_sink));
        manager.insert_order(bundle);
        manager.remove_bundle(replacement_data);
    }

    /// cancel should not notify anything
    #[test]
    fn test_cancel() {
        let mut data_gen = TestDataGenerator::new();
        let replacement_data = data_gen.create_bundle_replacement_data();
        let order_sink = MockOrderSink::new();
        let mut manager = OrderReplacementManager::new(Box::new(order_sink));
        manager.remove_bundle(replacement_data);
    }

    /// cancel before insert should not notify anything
    #[test]
    fn test_cancel_insert() {
        let mut data_gen = TestDataGenerator::new();
        let replacement_data = data_gen.create_bundle_replacement_data();
        let bundle = Order::Bundle(data_gen.create_bundle(Some(replacement_data)));
        let order_sink = MockOrderSink::new();

        let mut manager = OrderReplacementManager::new(Box::new(order_sink));
        manager.remove_bundle(replacement_data);
        manager.insert_order(bundle);
    }

    /// replacement with sequence increase should show both versions.
    #[test]
    fn test_increase_seq() {
        let mut data_gen = TestDataGenerator::new();
        let old_replacement_data = data_gen.create_bundle_replacement_data();
        let new_replacement_data = old_replacement_data.next();
        let old_bundle = Order::Bundle(data_gen.create_bundle(Some(old_replacement_data)));
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
        let old_bundle = Order::Bundle(data_gen.create_bundle(Some(old_replacement_data)));
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
}
