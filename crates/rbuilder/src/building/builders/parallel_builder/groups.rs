use crate::{
    building::evm_inspector::SlotKey,
    primitives::{OrderId, SimulatedOrder},
};
use ahash::{HashMap, HashSet};
use alloy_primitives::U256;
use itertools::Itertools;
use revm_primitives::Address;

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
    balance_reads: Vec<Address>,
    balance_writes: Vec<Address>,
    created_contracts: Vec<Address>,
    destructed_contracts: Vec<Address>,
    conflicting_group_ids: HashSet<usize>,
}

// if we're removing a group id from ConflictFinder.groups and adding its contents under some other group,
// we pass its id inside `removed_group_ids`
fn combine_groups(groups: Vec<GroupData>, removed_group_ids: Vec<usize>) -> GroupData {
    let mut orders = Vec::default();
    let mut reads = Vec::default();
    let mut writes = Vec::default();
    let mut balance_reads = Vec::default();
    let mut balance_writes = Vec::default();
    let mut created_contracts = Vec::default();
    let mut destructed_contracts = Vec::default();
    let mut conflicting_group_ids = removed_group_ids.into_iter().collect::<HashSet<usize>>();
    for group in groups {
        orders.extend(group.orders);
        reads.extend(group.reads);
        writes.extend(group.writes);
        balance_reads.extend(group.balance_reads);
        balance_writes.extend(group.balance_writes);
        created_contracts.extend(group.created_contracts);
        destructed_contracts.extend(group.destructed_contracts);
        conflicting_group_ids.extend(group.conflicting_group_ids);
    }
    reads.sort_unstable();
    reads.dedup();
    writes.sort_unstable();
    writes.dedup();
    balance_reads.sort_unstable();
    balance_reads.dedup();
    balance_writes.sort_unstable();
    balance_writes.dedup();
    created_contracts.sort_unstable();
    created_contracts.dedup();
    destructed_contracts.sort_unstable();
    destructed_contracts.dedup();

    GroupData{
        orders,
        reads,
        writes,
        balance_reads,
        balance_writes,
        created_contracts,
        destructed_contracts,
        conflicting_group_ids
    }
}

/// ConflictFinder is used to quickly find and update groups of orders that conflict with each other.
#[derive(Debug)]
pub struct ConflictFinder {
    group_counter: usize,
    group_reads: HashMap<SlotKey, Vec<usize>>,
    group_writes: HashMap<SlotKey, Vec<usize>>,
    group_balance_writes: HashMap<Address, Vec<usize>>,
    group_balance_reads: HashMap<Address, Vec<usize>>,
    group_contract_destructions: HashMap<Address, Vec<usize>>,
    group_contract_creations: HashMap<Address, Vec<usize>>,
    groups: HashMap<usize, GroupData>,
    orders: HashSet<OrderId>,
}

impl ConflictFinder {
    pub fn new() -> Self {
        ConflictFinder {
            group_counter: 0,
            group_reads: HashMap::default(),
            group_writes: HashMap::default(),
            group_balance_reads: HashMap::default(),
            group_balance_writes: HashMap::default(),
            group_contract_creations: HashMap::default(),
            group_contract_destructions: HashMap::default(),
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
                if let Some(group) = self.group_contract_destructions.get(&read_key.address) {
                    all_groups_in_conflict.extend_from_slice(group);
                }
            }
            for write_key in used_state.written_slot_values.keys() {
                if let Some(group) = self.group_reads.get(write_key) {
                    all_groups_in_conflict.extend_from_slice(group);
                }
                if let Some(group) = self.group_contract_destructions.get(&write_key.address) {
                    all_groups_in_conflict.extend_from_slice(group);
                }
            }
            // write_balance of current order vs read_balances of existing groups
            for write_balance_key in used_state.received_amount.keys().chain(used_state.sent_amount.keys()) {
                if let Some(group) = self.group_balance_reads.get(write_balance_key) {
                    all_groups_in_conflict.extend_from_slice(group);
                }
            }
            // read_balance of current order vs write_balances of existing groups
            for read_balance_key in used_state.read_balances.keys() {
                if let Some(group) = self.group_balance_writes.get(read_balance_key) {
                    all_groups_in_conflict.extend_from_slice(group);
                }
            }
            // 2 destruction txs for the same contract
            for destruction_address in &used_state.destructed_contracts {
                if let Some(group) = self.group_contract_destructions.get(destruction_address) {
                    all_groups_in_conflict.extend_from_slice(group);
                }
            }
            // TODO: not sure it's worth checking for
            // 2 creation txs for the same contract
            for creation_address in &used_state.created_contracts {
                if let Some(group) = self.group_contract_creations.get(creation_address) {
                    all_groups_in_conflict.extend_from_slice(group);
                }
            }
            all_groups_in_conflict.sort();
            all_groups_in_conflict.dedup();

            let current_order_group: GroupData = GroupData {
                orders: vec![order],
                reads: used_state.read_slot_values.into_keys().collect(),
                writes: used_state.written_slot_values.into_keys().collect(),
                balance_reads: used_state.read_balances.into_keys().collect(),
                balance_writes: used_state.sent_amount.into_keys().chain(used_state.received_amount.into_keys()).collect(),
                created_contracts: used_state.created_contracts,
                destructed_contracts: used_state.destructed_contracts,
                conflicting_group_ids: HashSet::default(),
            };

            match all_groups_in_conflict.len() {
                0 => {
                    // create new group with only one order in it
                    let group_id = self.group_counter;
                    self.group_counter += 1;
                    for read in &current_order_group.reads {
                        self.group_reads
                            .entry(read.clone())
                            .or_default()
                            .push(group_id);
                    }
                    for write in &current_order_group.writes {
                        self.group_writes
                            .entry(write.clone())
                            .or_default()
                            .push(group_id);
                    }
                    for balance_read in &current_order_group.balance_reads {
                        self.group_balance_reads
                            .entry(balance_read.clone())
                            .or_default()
                            .push(group_id);
                    }
                    for balance_write in &current_order_group.balance_writes {
                        self.group_balance_writes
                            .entry(balance_write.clone())
                            .or_default()
                            .push(group_id);
                    }
                    for created_contract in &current_order_group.created_contracts {
                        self.group_contract_creations
                            .entry(created_contract.clone())
                            .or_default()
                            .push(group_id);
                    }
                    for destructed_contract in &current_order_group.destructed_contracts {
                        self.group_contract_destructions
                            .entry(destructed_contract.clone())
                            .or_default()
                            .push(group_id);
                    }
                    self.groups.insert(group_id, current_order_group);
                }
                1 => {
                    // merge order into the group
                    let group_id = all_groups_in_conflict[0];
                    let other_group = self.groups.remove(&group_id).expect("group not found");
                    let combined_group = combine_groups(vec![current_order_group, other_group], vec![]);

                    for read in &combined_group.reads {
                        let group_reads_slot = self.group_reads.entry(read.clone()).or_default();
                        if !group_reads_slot.contains(&group_id) {
                            group_reads_slot.push(group_id);
                        }
                    }
                    for write in &combined_group.writes {
                        let group_writes_slot = self.group_writes.entry(write.clone()).or_default();
                        if !group_writes_slot.contains(&group_id) {
                            group_writes_slot.push(group_id);
                        }
                    }
                    for balance_read in &combined_group.balance_reads {
                        let groups_balance_read_address = self.group_balance_reads.entry(balance_read.clone()).or_default();
                        if !groups_balance_read_address.contains(&group_id) {
                            groups_balance_read_address.push(group_id);
                        }
                    }
                    for balance_write in &combined_group.balance_writes {
                        let groups_balance_write_address = self.group_balance_writes.entry(balance_write.clone()).or_default();
                        if !groups_balance_write_address.contains(&group_id) {
                            groups_balance_write_address.push(group_id);
                        }
                    }
                    for created_contract in &combined_group.created_contracts {
                        let groups_create_contract_address = self.group_contract_creations.entry(created_contract.clone()).or_default();
                        if !groups_create_contract_address.contains(&group_id) {
                            groups_create_contract_address.push(group_id);
                        }
                    }
                    for destructed_contract in &combined_group.destructed_contracts {
                        let groups_destruct_contract_address = self.group_contract_destructions.entry(destructed_contract.clone()).or_default();
                        if !groups_destruct_contract_address.contains(&group_id) {
                            groups_destruct_contract_address.push(group_id);
                        }
                    }
                    self.groups.insert(group_id, combined_group);
                }
                _ => {
                    // merge multiple group together and add new order there
                    let conflicting_groups = all_groups_in_conflict
                        .into_iter()
                        .map(|group_id| (group_id, self.groups.remove(&group_id).unwrap()))
                        .collect::<Vec<_>>();

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
                        for balance_read in &group_data.balance_reads {
                            let group_balance_reads_addr =
                                self.group_balance_reads.entry(balance_read.clone()).or_default();
                            if let Some(idx) = group_balance_reads_addr.iter().position(|el| el == group_id)
                            {
                                group_balance_reads_addr.swap_remove(idx);
                            }
                        }
                        for balance_write in &group_data.balance_writes {
                            let group_balance_writes_addr =
                                self.group_balance_writes.entry(balance_write.clone()).or_default();
                            if let Some(idx) = group_balance_writes_addr.iter().position(|el| el == group_id)
                            {
                                group_balance_writes_addr.swap_remove(idx);
                            }
                        }
                        for contract_creation in &group_data.created_contracts {
                            let group_contract_creations_addr =
                                self.group_contract_creations.entry(contract_creation.clone()).or_default();
                            if let Some(idx) =
                                group_contract_creations_addr.iter().position(|el| el == group_id)
                            {
                                group_contract_creations_addr.swap_remove(idx);
                            }
                        }
                        for contract_destruction in &group_data.destructed_contracts {
                            let group_contract_destructions_addr =
                                self.group_contract_destructions.entry(contract_destruction.clone()).or_default();
                            if let Some(idx) =
                                group_contract_destructions_addr.iter().position(|el| el == group_id)
                            {
                                group_contract_destructions_addr.swap_remove(idx);
                            }
                        }
                    }

                    let group_id = self.group_counter;
                    self.group_counter += 1;

                    let removed_group_ids = conflicting_groups.iter().map(|(id, _)| *id).collect();
                    let conflicting_groups = conflicting_groups
                        .into_iter()
                        .map(|(_, group)| group)
                        .chain(std::iter::once(current_order_group))
                        .collect();
                    let group_data = combine_groups(conflicting_groups, removed_group_ids);
                    
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
                    for balance_read in &group_data.balance_reads {
                        self.group_balance_reads
                            .entry(balance_read.clone())
                            .or_default()
                            .push(group_id);
                    }
                    for balance_write in &group_data.balance_writes {
                        self.group_balance_writes
                            .entry(balance_write.clone())
                            .or_default()
                            .push(group_id);
                    }
                    for contract_creation in &group_data.created_contracts {
                        self.group_contract_creations
                            .entry(contract_creation.clone())
                            .or_default()
                            .push(group_id);
                    }
                    for contract_destruction in &group_data.destructed_contracts {
                        self.group_contract_destructions
                            .entry(contract_destruction.clone())
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
    use alloy_primitives::{Address, TxHash, B256, U256};
    use reth::primitives::{
        Transaction, TransactionSigned, TransactionSignedEcRecovered, TxLegacy,
    };

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
