use ahash::HashMap;
use parking_lot::RwLock;
use rbuilder_primitives::Order;
use rbuilder_utils::replace_event_scheduler::{
    ReplaceEventScheduler, ReplaceEventSchedulerSubscription,
};
use std::sync::Arc;
use uuid::Uuid;

type BlockScheduler = ReplaceEventScheduler<Uuid, Option<Arc<Order>>>;
pub type PriorityUpdateSubscription = ReplaceEventSchedulerSubscription<Uuid, Option<Arc<Order>>>;

/// Ingress pool for already-validated priority updates.
///
/// Per-block schedulers store `Option<Arc<Order>>` keyed by `replacement_uuid`:
/// `Some` carries the decoded single-tx bundle, `None` is a cancellation.
/// Past-block pools are pruned by [`Self::head_updated`].
#[derive(Debug, Default, Clone)]
pub struct PriorityUpdateIngressOrderpool {
    pools_for_block: Arc<RwLock<HashMap<u64, BlockScheduler>>>,
}

impl PriorityUpdateIngressOrderpool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_event(&self, block_number: u64, uuid: Uuid, seq: u64, value: Option<Arc<Order>>) {
        let scheduler = self
            .pools_for_block
            .write()
            .entry(block_number)
            .or_default()
            .clone();
        scheduler.add_event(uuid, seq, value);
    }

    pub fn subscribe(&self, block_number: u64) -> PriorityUpdateSubscription {
        self.pools_for_block
            .write()
            .entry(block_number)
            .or_default()
            .subscribe()
    }

    pub fn head_updated(&self, new_block_number: u64) {
        self.pools_for_block
            .write()
            .retain(|block, _| *block > new_block_number);
    }
}
