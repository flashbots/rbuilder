use ahash::{HashMap, HashSet};
use alloy_primitives::{Address, B256, U256};
use itertools::Itertools;
use rbuilder_primitives::{evm_inspector::SlotKey, OrderId, SimulatedOrder};
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
    pub orders: Arc<Vec<Arc<SimulatedOrder>>>,
    pub conflicting_group_ids: Arc<HashSet<usize>>,
}

#[derive(Debug, Default)]
struct GroupData {
    orders: Vec<Arc<SimulatedOrder>>,
    reads: Vec<SlotKey>,
    writes: Vec<SlotKey>,
    balance_reads: Vec<Address>,
    balance_writes: Vec<Address>,
    code_writes: Vec<Address>,
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
    let mut code_writes = Vec::default();
    let mut conflicting_group_ids = removed_group_ids.into_iter().collect::<HashSet<usize>>();
    for group in groups {
        orders.extend(group.orders);
        reads.extend(group.reads);
        writes.extend(group.writes);
        balance_reads.extend(group.balance_reads);
        balance_writes.extend(group.balance_writes);
        code_writes.extend(group.code_writes);
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
    code_writes.sort_unstable();
    code_writes.dedup();

    GroupData {
        orders,
        reads,
        writes,
        balance_reads,
        balance_writes,
        code_writes,
        conflicting_group_ids,
    }
}

/// ConflictFinder is used to quickly find and update groups of orders that conflict with each other.
#[derive(Debug)]
pub struct ConflictFinder {
    group_counter: usize,
    group_reads: HashMap<Address, HashMap<B256, Vec<usize>>>, // mapping by `SlotKey`, but SlotKey.address and SlotKey.key are split into 2 separate HashMap keys
    group_writes: HashMap<Address, HashMap<B256, Vec<usize>>>, // same as above
    group_balance_reads: HashMap<Address, Vec<usize>>,
    group_balance_writes: HashMap<Address, Vec<usize>>,
    group_code_writes: HashMap<Address, Vec<usize>>,
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
            group_code_writes: HashMap::default(),
            groups: HashMap::default(),
            orders: HashSet::default(),
        }
    }

    pub fn add_orders(&mut self, orders: Vec<Arc<SimulatedOrder>>) {
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

            // check for all possible conflict types
            for read_key in used_state.read_slot_values.keys() {
                // reading from slot to which other order is writing to
                if let Some(inner_mapping) = self.group_writes.get(&read_key.address) {
                    if let Some(groups) = inner_mapping.get(&read_key.key) {
                        all_groups_in_conflict.extend_from_slice(groups);
                    }
                }
                // reading from slot on contract that other order is creating / destroying
                if let Some(group) = self.group_code_writes.get(&read_key.address) {
                    all_groups_in_conflict.extend_from_slice(group);
                }
            }
            for write_key in used_state.written_slot_values.keys() {
                // writing to slot other order is reading from
                if let Some(inner_mapping) = self.group_reads.get(&write_key.address) {
                    if let Some(groups) = inner_mapping.get(&write_key.key) {
                        all_groups_in_conflict.extend_from_slice(groups);
                    }
                }
                // writing to slot on contract that other order is creating / destroying
                if let Some(group) = self.group_code_writes.get(&write_key.address) {
                    all_groups_in_conflict.extend_from_slice(group);
                }
            }
            // writing balance other order is reading
            for write_balance_key in used_state
                .received_amount
                .keys()
                .chain(used_state.sent_amount.keys())
            {
                if let Some(group) = self.group_balance_reads.get(write_balance_key) {
                    all_groups_in_conflict.extend_from_slice(group);
                }
            }
            // reading balance other order is writing
            for read_balance_key in used_state.read_balances.keys() {
                if let Some(group) = self.group_balance_writes.get(read_balance_key) {
                    all_groups_in_conflict.extend_from_slice(group);
                }
            }
            for contract_addr in used_state
                .destructed_contracts
                .iter()
                .chain(used_state.created_contracts.iter())
            {
                // trying to create / destroy a contract on the same addr as other order
                if let Some(group) = self.group_code_writes.get(contract_addr) {
                    all_groups_in_conflict.extend_from_slice(group);
                }
                // trying to create / destroy a contract other order is trying to read from
                if let Some(inner_mapping) = self.group_reads.get(contract_addr) {
                    let inner_groups = inner_mapping.values().flatten();
                    all_groups_in_conflict.extend(inner_groups);
                }
                // trying to create / destroy a contract other order is trying to write to
                if let Some(inner_mapping) = self.group_writes.get(contract_addr) {
                    let inner_groups = inner_mapping.values().flatten();
                    all_groups_in_conflict.extend(inner_groups);
                }
            }
            all_groups_in_conflict.sort();
            all_groups_in_conflict.dedup();

            // create new group with only the new order in it
            let new_order_group: GroupData = {
                let mut balance_writes: Vec<Address> = used_state
                    .sent_amount
                    .into_keys()
                    .chain(used_state.received_amount.into_keys())
                    .collect();
                balance_writes.sort_unstable();
                balance_writes.dedup();

                let mut code_writes: Vec<Address> = used_state
                    .created_contracts
                    .into_iter()
                    .chain(used_state.destructed_contracts.into_iter())
                    .collect();
                code_writes.sort_unstable();
                code_writes.dedup();

                GroupData {
                    orders: vec![order],
                    reads: used_state.read_slot_values.into_keys().collect(),
                    writes: used_state.written_slot_values.into_keys().collect(),
                    balance_reads: used_state.read_balances.into_keys().collect(),
                    balance_writes,
                    code_writes,
                    conflicting_group_ids: HashSet::default(),
                }
            };

            match all_groups_in_conflict.len() {
                0 => {
                    // add `new_order_group` to index and `groups` under a new `group_id`
                    let group_id = self.group_counter;
                    self.group_counter += 1;
                    self.add_group_to_index(group_id, true, &new_order_group);
                    self.groups.insert(group_id, new_order_group);
                }
                1 => {
                    // combine `new_order_group` with the conflicting group under the conflicting group's `group_id`
                    let group_id = all_groups_in_conflict[0];
                    let other_group = self.groups.remove(&group_id).expect("group not found");
                    let combined_group = combine_groups(vec![new_order_group, other_group], vec![]);
                    self.add_group_to_index(group_id, false, &combined_group);
                    self.groups.insert(group_id, combined_group);
                }
                _ => {
                    // combine `new_order_group` with multiple conflicting groups under a new `group_id`
                    let conflicting_groups = all_groups_in_conflict
                        .into_iter()
                        .map(|group_id| (group_id, self.groups.remove(&group_id).unwrap()))
                        .collect::<Vec<_>>();

                    for (group_id, group_data) in &conflicting_groups {
                        self.remove_group_from_index(*group_id, group_data);
                    }

                    let group_id = self.group_counter;
                    self.group_counter += 1;

                    let removed_group_ids = conflicting_groups.iter().map(|(id, _)| *id).collect();
                    let conflicting_groups = conflicting_groups
                        .into_iter()
                        .map(|(_, group)| group)
                        .chain(std::iter::once(new_order_group))
                        .collect();
                    let combined_group = combine_groups(conflicting_groups, removed_group_ids);

                    self.add_group_to_index(group_id, true, &combined_group);
                    self.groups.insert(group_id, combined_group);
                }
            }
        }
    }

    fn add_group_to_index(&mut self, group_id: usize, is_new_id: bool, group_data: &GroupData) {
        for read in &group_data.reads {
            let address_reads = self.group_reads.entry(read.address).or_default();
            add_group_key_to_map(group_id, is_new_id, &read.key, address_reads);
        }
        for write in &group_data.writes {
            let address_writes = self.group_writes.entry(write.address).or_default();
            add_group_key_to_map(group_id, is_new_id, &write.key, address_writes);
        }
        add_group_to_map(
            group_id,
            is_new_id,
            &group_data.balance_reads,
            &mut self.group_balance_reads,
        );
        add_group_to_map(
            group_id,
            is_new_id,
            &group_data.balance_writes,
            &mut self.group_balance_writes,
        );
        add_group_to_map(
            group_id,
            is_new_id,
            &group_data.code_writes,
            &mut self.group_code_writes,
        );
    }

    fn remove_group_from_index(&mut self, group_id: usize, group_data: &GroupData) {
        for read in &group_data.reads {
            let address_reads = self.group_reads.entry(read.address).or_default();
            remove_group_key_from_map(group_id, &read.key, address_reads);
        }
        for write in &group_data.writes {
            let address_writes = self.group_writes.entry(write.address).or_default();
            remove_group_key_from_map(group_id, &write.key, address_writes);
        }
        remove_group_from_map(
            group_id,
            &group_data.balance_reads,
            &mut self.group_balance_reads,
        );
        remove_group_from_map(
            group_id,
            &group_data.balance_writes,
            &mut self.group_balance_writes,
        );
        remove_group_from_map(
            group_id,
            &group_data.code_writes,
            &mut self.group_code_writes,
        );
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

fn add_group_to_map<K: std::cmp::Eq + std::hash::Hash + Clone>(
    group_id: usize,
    is_new_id: bool,
    group_keys: &Vec<K>,
    map: &mut HashMap<K, Vec<usize>>,
) {
    for key in group_keys {
        add_group_key_to_map(group_id, is_new_id, key, map);
    }
}

// if the `group_id` is new, we don't check if the map already contains it
fn add_group_key_to_map<K: std::cmp::Eq + std::hash::Hash + Clone>(
    group_id: usize,
    is_new_id: bool,
    key: &K,
    map: &mut HashMap<K, Vec<usize>>,
) {
    let groups = map.entry(key.clone()).or_default();
    if is_new_id || !groups.contains(&group_id) {
        groups.push(group_id);
    }
}

fn remove_group_from_map<K: std::cmp::Eq + std::hash::Hash + Clone>(
    group_id: usize,
    group_keys: &Vec<K>,
    map: &mut HashMap<K, Vec<usize>>,
) {
    for key in group_keys {
        remove_group_key_from_map(group_id, key, map);
    }
}

fn remove_group_key_from_map<K: std::cmp::Eq + std::hash::Hash + Clone>(
    group_id: usize,
    key: &K,
    map: &mut HashMap<K, Vec<usize>>,
) {
    let groups = map.entry(key.clone()).or_default();
    if let Some(idx) = groups.iter().position(|el| *el == group_id) {
        groups.swap_remove(idx);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::TxLegacy;
    use alloy_primitives::{Address, TxHash, B256, U256};
    use reth::primitives::{Transaction, TransactionSigned};
    use reth_primitives::Recovered;

    use rbuilder_primitives::{
        evm_inspector::{SlotKey, UsedStateTrace},
        MempoolTx, Order, SimValue, SimulatedOrder, TransactionSignedEcRecoveredWithBlobs,
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

        pub fn create_tx(&mut self) -> Recovered<TransactionSigned> {
            Recovered::new_unchecked(
                TransactionSigned::new_unchecked(
                    Transaction::Legacy(TxLegacy::default()),
                    alloy_primitives::Signature::test_signature(),
                    self.create_hash(),
                ),
                Address::default(),
            )
        }

        pub fn create_order(
            &mut self,
            read: Option<&SlotKey>,
            write: Option<&SlotKey>,
            balance_read: Option<&Address>,
            balance_write: Option<&Address>,
            contract_creation: Option<&Address>,
            contract_destruction: Option<&Address>,
        ) -> Arc<SimulatedOrder> {
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
            if let Some(balance_read) = balance_read {
                trace
                    .read_balances
                    .insert(*balance_read, self.create_u256());
            }
            if let Some(balance_write) = balance_write {
                trace
                    .received_amount
                    .insert(*balance_write, self.create_u256());
                trace.sent_amount.insert(*balance_write, self.create_u256());
            }
            if let Some(contract_address) = contract_creation {
                trace.created_contracts.push(*contract_address);
            }
            if let Some(contract_address) = contract_destruction {
                trace.destructed_contracts.push(*contract_address);
            }

            Arc::new(SimulatedOrder {
                order: Order::Tx(MempoolTx {
                    tx_with_blobs: TransactionSignedEcRecoveredWithBlobs::new_no_blobs(
                        self.create_tx(),
                    )
                    .unwrap(),
                }),
                used_state_trace: Some(trace),
                sim_value: SimValue::default(),
            })
        }
    }

    #[test]
    fn two_writes_single_read() {
        let mut data_gen = DataGenerator::new();
        let slot = data_gen.create_slot();
        let oa = data_gen.create_order(None, Some(&slot), None, None, None, None);
        let ob = data_gen.create_order(None, Some(&slot), None, None, None, None);
        let oc = data_gen.create_order(Some(&slot), None, None, None, None, None);
        let mut cached_groups = ConflictFinder::new();
        cached_groups.add_orders(vec![oa, ob, oc]);
        let groups = cached_groups.get_order_groups();
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn two_reads() {
        let mut data_gen = DataGenerator::new();
        let slot = data_gen.create_slot();
        let oa = data_gen.create_order(Some(&slot), None, None, None, None, None);
        let ob = data_gen.create_order(Some(&slot), None, None, None, None, None);
        let mut cached_groups = ConflictFinder::new();
        cached_groups.add_orders(vec![oa, ob]);
        let groups = cached_groups.get_order_groups();
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn two_writes() {
        let mut data_gen = DataGenerator::new();
        let slot = data_gen.create_slot();
        let oa = data_gen.create_order(None, Some(&slot), None, None, None, None);
        let ob = data_gen.create_order(None, Some(&slot), None, None, None, None);
        let mut cached_groups = ConflictFinder::new();
        cached_groups.add_orders(vec![oa, ob]);
        let groups = cached_groups.get_order_groups();
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn two_balance_writes() {
        let mut data_gen = DataGenerator::new();
        let address = data_gen.create_slot().address;
        let oa = data_gen.create_order(None, None, None, Some(&address), None, None);
        let ob = data_gen.create_order(None, None, None, Some(&address), None, None);
        let mut cached_groups = ConflictFinder::new();
        cached_groups.add_orders(vec![oa, ob]);
        let groups = cached_groups.get_order_groups();
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn two_balance_reads() {
        let mut data_gen = DataGenerator::new();
        let address = data_gen.create_slot().address;
        let oa = data_gen.create_order(None, None, Some(&address), None, None, None);
        let ob = data_gen.create_order(None, None, Some(&address), None, None, None);
        let mut cached_groups = ConflictFinder::new();
        cached_groups.add_orders(vec![oa, ob]);
        let groups = cached_groups.get_order_groups();
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn two_balance_writes_single_read() {
        let mut data_gen = DataGenerator::new();
        let address = data_gen.create_slot().address;
        let oa = data_gen.create_order(None, None, None, Some(&address), None, None);
        let ob = data_gen.create_order(None, None, None, Some(&address), None, None);
        let oc = data_gen.create_order(None, None, Some(&address), None, None, None);
        let mut cached_groups = ConflictFinder::new();
        cached_groups.add_orders(vec![oa, ob, oc]);
        let groups = cached_groups.get_order_groups();
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn two_contract_destructions() {
        let mut data_gen = DataGenerator::new();
        let address = data_gen.create_slot().address;
        let oa = data_gen.create_order(None, None, None, None, None, Some(&address));
        let ob = data_gen.create_order(None, None, None, None, None, Some(&address));
        let mut cached_groups = ConflictFinder::new();
        cached_groups.add_orders(vec![oa, ob]);
        let groups = cached_groups.get_order_groups();
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn write_and_contract_destruction() {
        let mut data_gen = DataGenerator::new();
        let write_slot = data_gen.create_slot();
        let destruction_addr = write_slot.address;
        let oa = data_gen.create_order(None, Some(&write_slot), None, None, None, None);
        let ob = data_gen.create_order(None, None, None, None, None, Some(&destruction_addr));
        let mut cached_groups = ConflictFinder::new();
        cached_groups.add_orders(vec![oa, ob]);
        let groups = cached_groups.get_order_groups();
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn two_reads_one_destruction() {
        let mut data_gen = DataGenerator::new();
        let read_slot = data_gen.create_slot();
        let destruction_addr = read_slot.address;
        let oa = data_gen.create_order(Some(&read_slot), None, None, None, None, None);
        let ob = data_gen.create_order(Some(&read_slot), None, None, None, None, None);
        let oc = data_gen.create_order(None, None, None, None, None, Some(&destruction_addr));
        let mut cached_groups = ConflictFinder::new();
        cached_groups.add_orders(vec![oa, ob, oc]);
        let groups = cached_groups.get_order_groups();
        assert_eq!(groups.len(), 1);
    }
}
