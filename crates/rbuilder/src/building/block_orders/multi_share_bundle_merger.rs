use std::{cell::RefCell, rc::Rc, sync::Arc};

use ahash::HashMap;
use alloy_primitives::Address;
use tracing::error;

use rbuilder_primitives::{OrderId, SimulatedOrder};

use super::{share_bundle_merger::ShareBundleMerger, SimulatedOrderSink};

/// This struct allows us to have several parallel ShareBundleMerger merging bundles for a particular signer.
/// To do this we create a ShareBundleMerger for each address we have configured and a fallback ShareBundleMerger for any other signer.
/// In the future we may decide to remove this and merge EVERYTHING no matter who is sending.
#[derive(Debug)]
pub struct MultiShareBundleMerger<SinkType> {
    /// signer address -> merger
    signers_mergers: HashMap<Address, Rc<RefCell<ShareBundleMerger<SinkType>>>>,
    /// Any signer not in signers_mergers will be merged via fallback_merger
    fallback_merger: Rc<RefCell<ShareBundleMerger<SinkType>>>,
    /// We must take note of the signer we got on insert_order so we know where to forward on remove_order
    /// OrderId-> merger
    inserted_orders_signers: HashMap<OrderId, Option<Address>>,
}

impl<SinkType: SimulatedOrderSink> MultiShareBundleMerger<SinkType> {
    pub fn new(signers: &[Address], order_sink: Rc<RefCell<SinkType>>) -> Self {
        let mut signers_mergers = HashMap::default();
        for signer in signers {
            signers_mergers.insert(
                *signer,
                Rc::new(RefCell::new(ShareBundleMerger::new(order_sink.clone()))),
            );
        }
        Self {
            signers_mergers,
            inserted_orders_signers: Default::default(),
            fallback_merger: Rc::new(RefCell::new(ShareBundleMerger::new(order_sink.clone()))),
        }
    }

    fn get_merger(
        &mut self,
        signer: &Option<Address>,
    ) -> &Rc<RefCell<ShareBundleMerger<SinkType>>> {
        let merger = if let Some(signer) = signer {
            self.signers_mergers
                .get(signer)
                .unwrap_or(&self.fallback_merger)
        } else {
            &self.fallback_merger
        };
        merger
    }

    /// This cloning is tricky since we don't want to clone the Rcs we want to clone the real objects so the sink should be replaced.
    pub fn clone_with_sink(&self, order_sink: Rc<RefCell<SinkType>>) -> Self {
        let mut signers_mergers = HashMap::default();
        for (signer, merger) in &self.signers_mergers {
            signers_mergers.insert(
                *signer,
                Rc::new(RefCell::new(
                    merger.borrow().clone_with_sink(order_sink.clone()),
                )),
            );
        }
        Self {
            signers_mergers,
            inserted_orders_signers: self.inserted_orders_signers.clone(),
            fallback_merger: Rc::new(RefCell::new(
                self.fallback_merger
                    .borrow()
                    .clone_with_sink(order_sink.clone()),
            )),
        }
    }
}

impl<SinkType: SimulatedOrderSink> SimulatedOrderSink for MultiShareBundleMerger<SinkType> {
    fn insert_order(&mut self, order: Arc<SimulatedOrder>) {
        let signer = order.order.signer();
        self.inserted_orders_signers.insert(order.id(), signer);
        self.get_merger(&signer).borrow_mut().insert_order(order);
    }

    fn remove_order(&mut self, id: rbuilder_primitives::OrderId) -> Option<Arc<SimulatedOrder>> {
        match self.inserted_orders_signers.get(&id).cloned() {
            Some(signer) => self.get_merger(&signer).borrow_mut().remove_order(id),
            None => {
                error!(order_id = ?id, "remove_order for not inserted order");
                None
            }
        }
    }
}

#[cfg(test)]
mod test {
    use crate::building::block_orders::{order_dumper::OrderDumper, test_context::TestContext};

    use super::MultiShareBundleMerger;

    use alloy_primitives::Address;
    use lazy_static::lazy_static;
    lazy_static! {
        static ref SIGNER_1: Address = Address::random();
        static ref SIGNER_2: Address = Address::random();
        static ref UNKNOWN_SIGNER: Address = Address::random();
    }

    fn new_test_context() -> TestContext<MultiShareBundleMerger<OrderDumper>> {
        let signers = vec![*SIGNER_1, *SIGNER_2];
        TestContext::new(|dumper| MultiShareBundleMerger::new(&signers, dumper))
    }

    #[test]
    /// same signer bundles should go to the same megabundle
    fn test_same_signer() {
        let mut context = new_test_context();
        let (br_hi, br_low) = context.create_hi_low_orders(Some(*SIGNER_1), Some(*SIGNER_1));

        // first order generates a megabundle with it
        context.insert_order(br_hi.clone());
        let generated_order = context.pop_insert();
        context.assert_concatenated_sbundles_ok(&generated_order, &[br_hi.clone()]);

        // for second expect a cancellation and a new megabundle with both
        context.insert_order(br_low.clone());

        assert_eq!(context.pop_remove(), generated_order.id());
        let generated_order = context.pop_insert();
        context.assert_concatenated_sbundles_ok(&generated_order, &[br_hi.clone(), br_low.clone()]);
    }

    #[test]
    /// dif signer bundles should go to different megabundles
    fn test_different_signers() {
        let mut context = new_test_context();
        let (br_1, br_2) = context.create_hi_low_orders(Some(*SIGNER_1), Some(*SIGNER_2));
        let (_, br_3) = context.create_hi_low_orders(Some(*SIGNER_1), Some(*UNKNOWN_SIGNER));

        // first order generates a megabundle with it
        context.insert_order(br_1.clone());
        let generated_order = context.pop_insert();
        context.assert_concatenated_sbundles_ok(&generated_order, &[br_1.clone()]);

        // for second expect a new megabundle with it
        context.insert_order(br_2.clone());
        let generated_order = context.pop_insert();
        context.assert_concatenated_sbundles_ok(&generated_order, &[br_2.clone()]);

        // for an unknown signer expect a new megabundle with it
        context.insert_order(br_3.clone());
        let generated_order = context.pop_insert();
        context.assert_concatenated_sbundles_ok(&generated_order, &[br_3.clone()]);
    }

    #[test]
    /// unknown signer bundles should go to the same megabundle
    fn test_unknown_signers() {
        let mut context = new_test_context();
        let (br_hi, br_low) = context.create_hi_low_orders(Some(*UNKNOWN_SIGNER), None);

        // first order generates a megabundle with it
        context.insert_order(br_hi.clone());
        let generated_order = context.pop_insert();
        context.assert_concatenated_sbundles_ok(&generated_order, &[br_hi.clone()]);

        // for second expect a cancellation and a new megabundle with both
        context.insert_order(br_low.clone());

        assert_eq!(context.pop_remove(), generated_order.id());
        let generated_order = context.pop_insert();
        context.assert_concatenated_sbundles_ok(&generated_order, &[br_hi.clone(), br_low.clone()]);
    }
}
