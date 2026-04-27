use ahash::{HashMap, HashSet};
use alloy_primitives::U256;
use rbuilder_primitives::{evm_inspector::SlotKey, Order, OrderId, SimulatedOrder};
use std::sync::Arc;
use tracing::error;

use super::BlockState;
use pending_updates::PendingUpdates;

pub mod pending_updates;
pub mod pur_simulation_job;
pub mod simulate;

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
        }
        self.orders.insert(order_id, sim_order);
        evicted
    }

    pub fn apply_remove(&mut self, order_id: &OrderId) {
        self.pending.remove_order(order_id);
        self.orders.remove(order_id);
    }

    /// Priority-update orders that touch any of the slots in `read_slots`.
    ///
    /// For each read slot: if the slot was already written in the in-block
    /// bundle state, the PU is not needed for this slot — skip it. Otherwise
    /// surface the PU that owns the slot.
    pub fn get_updates(
        &self,
        current_block_state: &BlockState,
        read_slots: &[SlotKey],
    ) -> Vec<&Order> {
        if read_slots.is_empty() || self.orders.is_empty() {
            return Vec::new();
        }
        let mut matched: HashSet<OrderId> = HashSet::default();
        let mut result: Vec<&Order> = Vec::new();
        for slot in read_slots {
            if slot_overwritten_in_bundle(current_block_state, slot) {
                continue;
            }
            let Some(order_id) = self.pending.order_for_slot(slot) else {
                continue;
            };
            if !matched.insert(order_id) {
                continue;
            }
            if let Some(sim) = self.orders.get(&order_id) {
                result.push(sim.order.as_ref());
            }
        }
        result
    }
}

fn slot_overwritten_in_bundle(state: &BlockState, slot: &SlotKey) -> bool {
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
