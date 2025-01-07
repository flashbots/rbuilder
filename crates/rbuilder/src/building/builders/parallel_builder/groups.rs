use crate::{
    building::evm_inspector::SlotKey,
    primitives::{OrderId, SimulatedOrder},
};
use ahash::{HashMap, HashSet};
use alloy_primitives::U256;
use itertools::Itertools;

use std::sync::Arc;

/// ResolutionResult describes order of certain groups of orders.
#[derive(Debug, Default, Clone)]
pub struct ResolutionResult {
    /// Total coinbase profit of the given ordering.
    pub total_profit: U256,
    /// Sequence of orders and their profit in that sequence
    pub sequence_of_orders: Vec<(usize, U256)>,
}

/// ConflictGroups describes set of conflicting orders.
/// It's meant to be shared between thread who merges the group and who uses the best ordering to combine the result.
#[derive(Debug, Clone)]
pub struct ConflictGroup {
    pub id: usize,
    pub orders: Arc<Vec<SimulatedOrder>>,
    pub conflicting_group_ids: Arc<HashSet<usize>>,
}

#[derive(Debug, Default)]
struct GroupData {
    orders: Vec<SimulatedOrder>,
    reads: Vec<SlotKey>,
    writes: Vec<SlotKey>,
    conflicting_group_ids: HashSet<usize>,
}

/// ConflictFinder is used to quickly find and update groups of orders that conflict with each other.
#[derive(Debug)]
pub struct ConflictFinder {
    group_counter: usize,
    group_reads: HashMap<SlotKey, Vec<usize>>,
    group_writes: HashMap<SlotKey, Vec<usize>>,
    groups: HashMap<usize, GroupData>,
    orders: HashSet<OrderId>,
}

impl ConflictFinder {
    pub fn new() -> Self {
        ConflictFinder {
            group_counter: 0,
            group_reads: HashMap::default(),
            group_writes: HashMap::default(),
            groups: HashMap::default(),
            orders: HashSet::default(),
        }
    }

    pub fn add_orders(&mut self, orders: Vec<SimulatedOrder>) {
        for order in orders {
            if self.orders.contains(&order.id()) {
                continue;
            }
            self.orders.insert(order.id());

            let used_state = if let Some(used_state) = &order.used_state_trace {
                used_state.clone()
            } else {
                continue;
            };

            let mut all_groups_in_conflict = Vec::new();

            for read_key in used_state.read_slot_values.keys() {
                if let Some(group) = self.group_writes.get(read_key) {
                    all_groups_in_conflict.extend_from_slice(group);
                }
            }
            for write_key in used_state.written_slot_values.keys() {
                if let Some(group) = self.group_reads.get(write_key) {
                    all_groups_in_conflict.extend_from_slice(group);
                }
            }
            all_groups_in_conflict.sort();
            all_groups_in_conflict.dedup();

            match all_groups_in_conflict.len() {
                0 => {
                    // create new group with only one order in it
                    let group_id = self.group_counter;
                    self.group_counter += 1;
                    let group_data = GroupData {
                        orders: vec![order],
                        reads: used_state.read_slot_values.keys().cloned().collect(),
                        writes: used_state.written_slot_values.keys().cloned().collect(),
                        conflicting_group_ids: HashSet::default(),
                    };
                    for read in &group_data.reads {
                        self.group_reads
                            .entry(read.clone())
                            .or_default()
                            .push(group_id);
                    }
                    for write in &group_data.writes {
                        self.group_writes
                            .entry(write.clone())
                            .or_default()
                            .push(group_id);
                    }
                    self.groups.insert(group_id, group_data);
                }
                1 => {
                    // merge order into the group
                    let group_id = all_groups_in_conflict[0];
                    let group_data = self.groups.get_mut(&group_id).expect("group not found");
                    group_data.orders.push(order);
                    for read in used_state.read_slot_values.keys() {
                        group_data.reads.push(read.clone());
                    }
                    group_data.reads.sort();
                    group_data.reads.dedup();
                    for write in used_state.written_slot_values.keys() {
                        group_data.writes.push(write.clone());
                    }
                    group_data.writes.sort();
                    group_data.writes.dedup();
                    for read in &group_data.reads {
                        let group_reads_slot = self.group_reads.entry(read.clone()).or_default();
                        if !group_reads_slot.contains(&group_id) {
                            group_reads_slot.push(group_id);
                        }
                    }
                    for write in &group_data.writes {
                        let group_writes_slot = self.group_writes.entry(write.clone()).or_default();
                        if !group_writes_slot.contains(&group_id) {
                            group_writes_slot.push(group_id);
                        }
                    }
                }
                _ => {
                    // merge multiple group together and add new order there
                    let conflicting_groups = all_groups_in_conflict
                        .into_iter()
                        .map(|group_id| (group_id, self.groups.remove(&group_id).unwrap()))
                        .collect::<Vec<_>>();

                    // Collect all conflicting group IDs
                    let merged_conflicting_ids = conflicting_groups
                        .iter()
                        .flat_map(|(gid, gd)| {
                            gd.conflicting_group_ids
                                .iter()
                                .cloned()
                                .chain(std::iter::once(*gid))
                        })
                        .collect::<HashSet<_>>();

                    for (group_id, group_data) in &conflicting_groups {
                        for read in &group_data.reads {
                            let group_reads_slot =
                                self.group_reads.entry(read.clone()).or_default();
                            if let Some(idx) = group_reads_slot.iter().position(|el| el == group_id)
                            {
                                group_reads_slot.swap_remove(idx);
                            }
                        }
                        for write in &group_data.writes {
                            let group_writes_slot =
                                self.group_writes.entry(write.clone()).or_default();
                            if let Some(idx) =
                                group_writes_slot.iter().position(|el| el == group_id)
                            {
                                group_writes_slot.swap_remove(idx);
                            }
                        }
                    }

                    let group_id = self.group_counter;
                    self.group_counter += 1;

                    let mut group_data = GroupData {
                        orders: vec![order],
                        reads: used_state.read_slot_values.keys().cloned().collect(),
                        writes: used_state.written_slot_values.keys().cloned().collect(),
                        conflicting_group_ids: merged_conflicting_ids,
                    };
                    for (_, mut group) in conflicting_groups {
                        group_data.orders.append(&mut group.orders);
                        group_data.reads.append(&mut group.reads);
                        group_data.writes.append(&mut group.writes);
                        group_data
                            .conflicting_group_ids
                            .extend(group.conflicting_group_ids);
                    }
                    group_data.reads.sort();
                    group_data.reads.dedup();
                    group_data.writes.sort();
                    group_data.writes.dedup();
                    for read in &group_data.reads {
                        self.group_reads
                            .entry(read.clone())
                            .or_default()
                            .push(group_id);
                    }
                    for write in &group_data.writes {
                        self.group_writes
                            .entry(write.clone())
                            .or_default()
                            .push(group_id);
                    }
                    self.groups.insert(group_id, group_data);
                }
            }
        }
    }

    pub fn get_order_groups(&self) -> Vec<ConflictGroup> {
        self.groups
            .iter()
            .sorted_by_key(|(idx, _)| *idx)
            .map(|(group_id, group_data)| ConflictGroup {
                id: *group_id,
                orders: Arc::new(group_data.orders.clone()),
                conflicting_group_ids: Arc::new(group_data.conflicting_group_ids.clone()),
            })
            .collect()
    }
}

impl Default for ConflictFinder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::TxLegacy;
    use alloy_primitives::{Address, TxHash, B256, U256};
    use reth::primitives::{Transaction, TransactionSigned, TransactionSignedEcRecovered};

    use crate::{
        building::evm_inspector::{SlotKey, UsedStateTrace},
        primitives::{
            MempoolTx, Order, SimValue, SimulatedOrder, TransactionSignedEcRecoveredWithBlobs,
        },
    };

    use super::ConflictFinder;

    struct DataGenerator {
        last_used_id: u64,
    }
    impl DataGenerator {
        pub fn new() -> DataGenerator {
            DataGenerator { last_used_id: 0 }
        }

        pub fn create_u64(&mut self) -> u64 {
            self.last_used_id += 1;
            self.last_used_id
        }

        pub fn create_u256(&mut self) -> U256 {
            U256::from(self.create_u64())
        }

        pub fn create_b256(&mut self) -> B256 {
            B256::from(self.create_u256())
        }

        pub fn create_hash(&mut self) -> TxHash {
            TxHash::from(self.create_u256())
        }

        pub fn create_slot(&mut self) -> SlotKey {
            SlotKey {
                address: Address::ZERO,
                key: self.create_b256(),
            }
        }

        pub fn create_tx(&mut self) -> TransactionSignedEcRecovered {
            TransactionSignedEcRecovered::from_signed_transaction(
                TransactionSigned {
                    hash: self.create_hash(),
                    transaction: Transaction::Legacy(TxLegacy::default()),
                    ..Default::default()
                },
                Address::default(),
            )
        }

        pub fn create_order(
            &mut self,
            read: Option<&SlotKey>,
            write: Option<&SlotKey>,
        ) -> SimulatedOrder {
            let mut trace = UsedStateTrace::default();
            if let Some(read) = read {
                trace
                    .read_slot_values
                    .insert(read.clone(), self.create_b256());
            }
            if let Some(write) = write {
                trace
                    .written_slot_values
                    .insert(write.clone(), self.create_b256());
            }

            SimulatedOrder {
                order: Order::Tx(MempoolTx {
                    tx_with_blobs: TransactionSignedEcRecoveredWithBlobs::new_no_blobs(
                        self.create_tx(),
                    )
                    .unwrap(),
                }),
                used_state_trace: Some(trace),
                sim_value: SimValue::default(),
                prev_order: None,
            }
        }
    }

    #[test]
    fn two_writes_single_read() {
        let mut data_gen = DataGenerator::new();
        let slot = data_gen.create_slot();
        let oa = data_gen.create_order(None, Some(&slot));
        let ob = data_gen.create_order(None, Some(&slot));
        let oc = data_gen.create_order(Some(&slot), None);
        let mut cached_groups = ConflictFinder::new();
        cached_groups.add_orders(vec![oa, ob, oc]);
        let groups = cached_groups.get_order_groups();
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn two_reads() {
        let mut data_gen = DataGenerator::new();
        let slot = data_gen.create_slot();
        let oa = data_gen.create_order(Some(&slot), None);
        let ob = data_gen.create_order(Some(&slot), None);
        let mut cached_groups = ConflictFinder::new();
        cached_groups.add_orders(vec![oa, ob]);
        let groups = cached_groups.get_order_groups();
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn two_writes() {
        let mut data_gen = DataGenerator::new();
        let slot = data_gen.create_slot();
        let oa = data_gen.create_order(None, Some(&slot));
        let ob = data_gen.create_order(None, Some(&slot));
        let mut cached_groups = ConflictFinder::new();
        cached_groups.add_orders(vec![oa, ob]);
        let groups = cached_groups.get_order_groups();
        assert_eq!(groups.len(), 2);
    }
}
