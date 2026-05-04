use ahash::{HashMap, HashSet};
use alloy_primitives::U256;
use rbuilder_primitives::{evm_inspector::SlotKey, OrderId, PriorityUpdateClass, SimulatedOrder};
use std::sync::Arc;
use tracing::error;

use super::BlockState;
use pending_updates::PendingUpdates;

pub mod pending_updates;
pub mod priority_update_pool;
pub mod pur_simulation_job;
pub mod simulate;
pub mod used_priority_update_tracer;

/// Holds the set of simulated priority-update orders active for the current block.
///
/// Used by three kinds of consumers:
/// 1. The PUR simulation thread (tracks its own emitted PUs for bookkeeping).
/// 2. Each simulation worker thread (so that in-flight simulations can read the
///    merged overlay).
/// 3. Each builder thread (so that [`Self::get_updates`] returns the PUs that
///    should run alongside an order at commit time).
#[derive(Debug, Default, Clone)]
pub struct PriorityUpdatePool {
    pending: PendingUpdates,
    orders: HashMap<OrderId, Arc<SimulatedOrder>>,
    /// Orders classified as [`PriorityUpdateClass::ForceTopOfBlock`]. These are
    /// committed at the top of every built block in addition to participating
    /// in the regular PU overlay.
    force_top_of_block: HashMap<OrderId, Arc<SimulatedOrder>>,
}

impl PriorityUpdatePool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Aggregated PU state for use as a simulation overlay.
    pub fn pending_update_state(&self) -> &PendingUpdates {
        &self.pending
    }

    /// Merges a simulated priority update into the pool. Orders whose
    /// storage writes conflict with the new one are evicted and their ids
    /// returned.
    pub fn apply_update(&mut self, sim_order: Arc<SimulatedOrder>) -> Vec<OrderId> {
        let Some(pu_data) = sim_order.pu_data.clone() else {
            error!(order_id = ?sim_order.id(), "apply_update called with non-PU simulated order");
            return Vec::new();
        };
        let order_id = sim_order.id();
        let evicted = self
            .pending
            .add_new_simulated_update(order_id, pu_data.changeset);
        for id in &evicted {
            self.orders.remove(id);
            self.force_top_of_block.remove(id);
        }
        if matches!(
            sim_order.order.metadata().priority_update_data,
            Some(PriorityUpdateClass::ForceTopOfBlock)
        ) {
            self.force_top_of_block
                .insert(order_id, Arc::clone(&sim_order));
        }
        self.orders.insert(order_id, sim_order);
        evicted
    }

    pub fn apply_remove(&mut self, order_id: &OrderId) {
        self.pending.remove_order(order_id);
        self.orders.remove(order_id);
        self.force_top_of_block.remove(order_id);
    }

    /// Orders that must be committed at the top of every built block, sorted
    /// by [`OrderId`] for deterministic inclusion order across builders. The
    /// builder iterates this list once at the start of `build_block` and
    /// commits each before the regular order loop runs.
    pub fn force_top_of_block_orders(&self) -> Vec<Arc<SimulatedOrder>> {
        let mut orders: Vec<_> = self.force_top_of_block.values().cloned().collect();
        orders.sort_by_key(|sim| sim.id());
        orders
    }

    /// Priority-update orders owning any of the slots in `read_slots`. The
    /// caller is adviced to filter out slots already written in the current
    /// bundle state via [`select_unwritten_slots`] beforehand.
    pub fn get_updates(&self, read_slots: &[SlotKey]) -> Vec<Arc<SimulatedOrder>> {
        if read_slots.is_empty() || self.orders.is_empty() {
            return Vec::new();
        }
        let mut matched: HashSet<OrderId> = HashSet::default();
        let mut result: Vec<Arc<SimulatedOrder>> = Vec::new();
        for slot in read_slots {
            let Some(order_id) = self.pending.order_for_slot(slot) else {
                continue;
            };
            if !matched.insert(order_id) {
                continue;
            }
            if let Some(sim) = self.orders.get(&order_id) {
                result.push(Arc::clone(sim));
            }
        }
        result
    }
}

/// Returns the subset of `slots` whose address/key has not been written in
/// the current in-block bundle state. Used as a prefilter before
/// [`PriorityUpdatePool::get_updates`] so already-overwritten slots don't
/// pull in PUs that are no longer needed.
pub fn select_unwritten_slots<DB>(state: &BlockState<DB>, slots: &[SlotKey]) -> Vec<SlotKey> {
    slots
        .iter()
        .filter(|slot| !slot_overwritten_in_bundle(state, slot))
        .cloned()
        .collect()
}

fn slot_overwritten_in_bundle<DB>(state: &BlockState<DB>, slot: &SlotKey) -> bool {
    let Some(account) = state.bundle_state().state.get(&slot.address) else {
        return false;
    };
    let key = U256::from_be_bytes(slot.key.0);
    account
        .storage
        .get(&key)
        .map(|s| s.is_changed())
        .unwrap_or(false)
}
