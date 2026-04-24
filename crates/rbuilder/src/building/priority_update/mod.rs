use rbuilder_primitives::{evm_inspector::SlotKey, Order};

use super::BlockState;

pub mod pending_updates;
pub mod pur_simulation_job;
pub mod simulate;

#[derive(Debug, Default, Clone)]
pub struct PriorityUpdatePool {}

impl PriorityUpdatePool {
    /// Priority-update orders that touch any of the slots in `read_slots`.
    pub fn get_updates(
        &self,
        _current_block_state: &BlockState,
        _read_slots: &[SlotKey],
    ) -> Vec<&Order> {
        Vec::new()
    }
}
