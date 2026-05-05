use ahash::{HashMap, HashSet};
use alloy_primitives::U256;
use rbuilder_primitives::{evm_inspector::SlotKey, PriorityUpdateKind, SimulatedOrder};
use std::sync::Arc;
use tracing::error;
use uuid::Uuid;

use super::BlockState;
use pending_updates::PendingUpdates;

pub mod pending_updates;
pub mod priority_update_pool;
pub mod pur_simulation_job;
pub mod simulate;
pub mod used_priority_update_tracer;

/// Holds the set of simulated priority-update orders active for the current block.
///
/// Indexed by the priority-update `replacement_uuid`. Each entry remembers the
/// scheduler seq it was added with so storage-conflict eviction emissions can
/// reuse that seq when overwriting the entry on the result scheduler.
#[derive(Debug, Default, Clone)]
pub struct PriorityUpdatePool {
    pending: PendingUpdates,
    orders: HashMap<Uuid, (u64, Arc<SimulatedOrder>)>,
    /// Orders classified as [`PriorityUpdateKind::ForceTopOfBlock`]. These are
    /// committed at the top of every built block in addition to participating
    /// in the regular PU overlay.
    force_top_of_block: HashMap<Uuid, Arc<SimulatedOrder>>,
}

impl PriorityUpdatePool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Aggregated PU state for use as a simulation overlay.
    pub fn pending_update_state(&self) -> &PendingUpdates {
        &self.pending
    }

    /// Apply a single keyed event. `Some(sim)` installs/replaces the order
    /// stored under `uuid` at the given `seq`; `None` drops it. Returns
    /// `(evicted_uuid, evicted_seq)` for orders evicted by storage-slot
    /// conflicts caused by the new sim — the seq is the one the evicted entry
    /// was added with, so callers can reuse it to overwrite the result
    /// scheduler's stored value. Same-uuid replacements are NOT included in
    /// the returned list.
    pub fn apply_event(
        &mut self,
        uuid: Uuid,
        seq: u64,
        maybe_sim: Option<Arc<SimulatedOrder>>,
    ) -> Vec<(Uuid, u64)> {
        match maybe_sim {
            Some(sim_order) => {
                let Some(pu_data) = sim_order.pu_data.clone() else {
                    error!(?uuid, "apply_event called with non-PU simulated order");
                    return Vec::new();
                };
                // Replace prior version of this uuid (if any) before adding the new one.
                self.pending.remove_order(&uuid);
                self.orders.remove(&uuid);
                self.force_top_of_block.remove(&uuid);

                let evicted_uuids = self
                    .pending
                    .add_new_simulated_update(uuid, pu_data.changeset);
                let mut evicted = Vec::with_capacity(evicted_uuids.len());
                for id in evicted_uuids {
                    self.force_top_of_block.remove(&id);
                    if let Some((evicted_seq, _)) = self.orders.remove(&id) {
                        evicted.push((id, evicted_seq));
                    }
                }
                if matches!(
                    sim_order
                        .order
                        .metadata()
                        .priority_update_data
                        .as_ref()
                        .map(|d| d.kind),
                    Some(PriorityUpdateKind::ForceTopOfBlock)
                ) {
                    self.force_top_of_block.insert(uuid, Arc::clone(&sim_order));
                }
                self.orders.insert(uuid, (seq, sim_order));
                evicted
            }
            None => {
                self.pending.remove_order(&uuid);
                self.orders.remove(&uuid);
                self.force_top_of_block.remove(&uuid);
                Vec::new()
            }
        }
    }

    /// Convenience wrapper around [`Self::apply_event`] for batch input.
    pub fn apply_events<I>(&mut self, events: I) -> Vec<(Uuid, u64)>
    where
        I: IntoIterator<Item = (Uuid, u64, Option<Arc<SimulatedOrder>>)>,
    {
        let mut evicted = Vec::new();
        for (uuid, seq, maybe_sim) in events {
            evicted.extend(self.apply_event(uuid, seq, maybe_sim));
        }
        evicted
    }

    /// Orders that must be committed at the top of every built block, sorted
    /// by uuid for deterministic inclusion order across builders. The
    /// builder iterates this list once at the start of `build_block` and
    /// commits each before the regular order loop runs.
    pub fn force_top_of_block_orders(&self) -> Vec<Arc<SimulatedOrder>> {
        let mut entries: Vec<_> = self.force_top_of_block.iter().collect();
        entries.sort_by_key(|(uuid, _)| **uuid);
        entries
            .into_iter()
            .map(|(_, sim)| Arc::clone(sim))
            .collect()
    }

    /// Priority-update orders owning any of the slots in `read_slots`. The
    /// caller is adviced to filter out slots already written in the current
    /// bundle state via [`select_unwritten_slots`] beforehand.
    pub fn get_updates(&self, read_slots: &[SlotKey]) -> Vec<Arc<SimulatedOrder>> {
        if read_slots.is_empty() || self.orders.is_empty() {
            return Vec::new();
        }
        let mut matched: HashSet<Uuid> = HashSet::default();
        let mut result: Vec<Arc<SimulatedOrder>> = Vec::new();
        for slot in read_slots {
            let Some(uuid) = self.pending.order_for_slot(slot) else {
                continue;
            };
            if !matched.insert(uuid) {
                continue;
            }
            if let Some((_, sim)) = self.orders.get(&uuid) {
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
