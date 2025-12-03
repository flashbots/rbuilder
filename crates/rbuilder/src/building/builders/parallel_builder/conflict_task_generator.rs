use ahash::{HashMap, HashSet};
use alloy_primitives::{utils::format_ether, U256};
use crossbeam_queue::SegQueue;
use itertools::Itertools;
use rbuilder_primitives::SimulatedOrder;
use std::{sync::Arc, time::Instant};
use tracing::trace;

use super::{
    task::ConflictTask, Algorithm, ConflictGroup, ConflictResolutionResultPerGroup, GroupId,
    ResolutionResult, TaskPriority, TaskQueue,
};
use std::sync::mpsc as std_mpsc;

const THRESHOLD_FOR_SIGNIFICANT_CHANGE: u64 = 20;
const NUMBER_OF_TOP_ORDERS_TO_CONSIDER_FOR_SIGNIFICANT_CHANGE: usize = 10;
const MAX_LENGTH_FOR_ALL_PERMUTATIONS: usize = 3;
const NUMBER_OF_RANDOM_TASKS: usize = 50;

/// Manages conflicts and updates for conflict groups, coordinating with a worker pool to process tasks.
pub struct ConflictTaskGenerator {
    existing_groups: HashMap<GroupId, ConflictGroup>,
    task_queue: TaskQueue,
    group_result_sender: std_mpsc::Sender<ConflictResolutionResultPerGroup>,
    safe_sorting_only: bool,
}

impl ConflictTaskGenerator {
    /// Creates a new [ConflictTaskGenerator] instance responsible for generating and managing tasks for conflict groups.
    ///
    /// # Arguments
    ///
    /// * `task_queue` - The queue to store the generated tasks.
    /// * `group_result_sender` - The sender to send the results of the conflict resolution.
    pub fn new(
        safe_sorting_only: bool,
        task_queue: TaskQueue,
        group_result_sender: std_mpsc::Sender<ConflictResolutionResultPerGroup>,
    ) -> Self {
        Self {
            safe_sorting_only,
            existing_groups: HashMap::default(),
            task_queue,
            group_result_sender,
        }
    }

    /// Processes a vector of new conflict groups, updating existing groups or creating new tasks as necessary.
    ///
    /// # Arguments
    ///
    /// * `new_groups` - A vector of new [ConflictGroup]s to process.
    pub fn process_groups(&mut self, new_groups: Vec<ConflictGroup>) {
        let mut sorted_groups = new_groups;
        sorted_groups.sort_by(|a, b| b.orders.len().cmp(&a.orders.len()));

        let mut processed_groups = HashSet::default();
        for new_group in sorted_groups {
            // If the group has already been processed, skip it
            if self.has_been_processed(&new_group.id, &processed_groups) {
                continue;
            }

            // Process the group
            self.process_single_group(new_group.clone());

            // Add the new group and all its subsets to the processed groups
            self.add_processed_groups(&new_group, &mut processed_groups);

            // Remove all subset groups
            if !new_group.conflicting_group_ids.is_empty() {
                self.remove_conflicting_subset_groups(&new_group);
            }
        }
    }

    /// Adds the group and all the conflicting group subsets to the processed groups
    fn add_processed_groups(
        &mut self,
        new_group: &ConflictGroup,
        processed_groups: &mut HashSet<GroupId>,
    ) {
        // Add all subset groups to the processed groups
        let subset_ids: Vec<GroupId> = self
            .existing_groups
            .keys()
            .filter(|&id| new_group.conflicting_group_ids.contains(id))
            .cloned()
            .collect();
        for id in subset_ids {
            processed_groups.insert(id);
        }

        // Add the new group to the processed groups
        processed_groups.insert(new_group.id);
    }

    /// Checks if a group has been processed
    fn has_been_processed(&self, group_id: &GroupId, processed_groups: &HashSet<GroupId>) -> bool {
        processed_groups.contains(group_id)
    }

    /// Removes the subset groups from the existing groups and cancels the tasks for those groups
    fn remove_conflicting_subset_groups(&mut self, superset_group: &ConflictGroup) {
        let subset_ids: Vec<GroupId> = self
            .existing_groups
            .keys()
            .filter(|&id| superset_group.conflicting_group_ids.contains(id))
            .cloned()
            .collect();

        trace!(groups = ?subset_ids,"Removing subset groups");
        for id in subset_ids {
            self.existing_groups.remove(&id);
            self.cancel_tasks_for_group(id);
        }
    }

    /// Processes a single order group, determining whether to update an existing group or create new tasks.
    ///
    /// This method checks if the group has changed since it was last processed. If there are no changes,
    /// it skips further processing. For changed groups, it determines the appropriate priority and
    /// either updates existing tasks or creates new ones.
    ///
    /// # Arguments
    ///
    /// * `new_group` - The new `ConflictGroup` to process.
    fn process_single_group(&mut self, new_group: ConflictGroup) {
        let group_id = new_group.id;

        let should_process = match self.existing_groups.get(&group_id) {
            Some(existing_group) => self.group_has_changed(&new_group, existing_group),
            None => true,
        };

        if should_process {
            if new_group.orders.len() == 1 {
                self.process_single_order_group(group_id, &new_group);
            } else {
                self.process_multi_order_group(group_id, &new_group);
            }
            self.existing_groups.insert(group_id, new_group);
        }
    }

    /// Processes a multi-order group, determining whether to update existing tasks or create new ones and the corresponding priority
    /// We give updates higher priority if they are significant changes
    ///
    /// # Arguments
    ///
    /// * `group_id` - The ID of the group to process.
    /// * `new_group` - The new `ConflictGroup` to process.
    fn process_multi_order_group(&mut self, group_id: GroupId, new_group: &ConflictGroup) {
        let priority = if let Some(existing_group) = self.existing_groups.get(&group_id) {
            if self.has_significant_changes(new_group, existing_group) {
                TaskPriority::High
            } else {
                TaskPriority::Medium
            }
        } else {
            TaskPriority::High
        };
        trace!(
            group = group_id,
            order_count = new_group.orders.len(),
            profit =
                format_ether(self.sum_top_n_profits(&new_group.orders, new_group.orders.len())),
            priority = priority.display(),
            "Processing multi order group"
        );
        if self.existing_groups.contains_key(&group_id) {
            self.update_tasks(group_id, new_group, priority);
        } else {
            self.create_new_tasks(new_group, priority);
        }
    }

    /// Processes a single order group, sending the result to the worker pool.
    ///
    /// # Arguments
    ///
    /// * `group_id` - The ID of the group to process.
    /// * `group` - The `ConflictGroup` to process.
    fn process_single_order_group(&mut self, group_id: GroupId, group: &ConflictGroup) {
        let sequence_of_orders = ResolutionResult {
            total_profit: group.orders[0]
                .sim_value
                .full_profit_info()
                .coinbase_profit(),
            sequence_of_orders: vec![(
                0,
                group.orders[0]
                    .sim_value
                    .full_profit_info()
                    .coinbase_profit(),
            )],
        };
        // We ignore the error since it means "receiver disconnected" and we expect the caller will detect the cancellation and stop calling us.
        let _ = self
            .group_result_sender
            .send((group_id, (sequence_of_orders, group.clone())));
    }

    /// Determines if there are any changes between a new group and an existing group.
    ///
    /// This method compares the number of orders and each individual order in both groups
    /// to detect any changes.
    ///
    /// # Arguments
    ///
    /// * `new_group` - The new `ConflictGroup` to compare.
    /// * `existing_group` - The existing `ConflictGroup` to compare against.
    ///
    /// # Returns
    ///
    /// `true` if there are any changes between the groups, `false` otherwise.
    fn group_has_changed(&self, new_group: &ConflictGroup, existing_group: &ConflictGroup) -> bool {
        // Compare the number of orders
        if new_group.orders.len() != existing_group.orders.len() {
            return true;
        }

        // Compare each order in the groups
        for (new_order, existing_order) in new_group.orders.iter().zip(existing_group.orders.iter())
        {
            if new_order != existing_order {
                return true;
            }
        }

        false
    }

    /// Determines if there are significant changes between a new group and an existing group.
    ///
    /// # Arguments
    ///
    /// * `new_group` - The new `ConflictGroup` to compare.
    /// * `existing_group` - The existing `ConflictGroup` to compare against.
    ///
    /// # Returns
    ///
    /// `true` if there are significant changes, `false` otherwise.
    fn has_significant_changes(
        &self,
        new_group: &ConflictGroup,
        existing_group: &ConflictGroup,
    ) -> bool {
        let new_top_profit = self.sum_top_n_profits(
            &new_group.orders,
            NUMBER_OF_TOP_ORDERS_TO_CONSIDER_FOR_SIGNIFICANT_CHANGE,
        );
        let existing_top_profit = self.sum_top_n_profits(
            &existing_group.orders,
            NUMBER_OF_TOP_ORDERS_TO_CONSIDER_FOR_SIGNIFICANT_CHANGE,
        );

        if new_top_profit == existing_top_profit {
            return false;
        }

        if new_top_profit > existing_top_profit {
            // if new_top_profit is more than 20% higher than existing_top_profit, we consider it a significant change
            let threshold = existing_top_profit * U256::from(THRESHOLD_FOR_SIGNIFICANT_CHANGE)
                / U256::from(100);
            if new_top_profit - existing_top_profit > threshold {
                return true;
            }
        }
        false
    }

    /// Calculates the sum of the top N profits from a slice of simulated orders.
    ///
    /// # Arguments
    ///
    /// * `orders` - A slice of `SimulatedOrder`s to process.
    /// * `n` - The number of top profits to sum.
    ///
    /// # Returns
    ///
    /// The sum of the top N profits as a `U256`.
    fn sum_top_n_profits(&self, orders: &[Arc<SimulatedOrder>], n: usize) -> U256 {
        orders
            .iter()
            .map(|o| o.sim_value.full_profit_info().coinbase_profit())
            .sorted_by(|a, b| b.cmp(a))
            .take(n)
            .sum()
    }

    /// Updates tasks for a given group, cancelling existing tasks and creating new ones.
    ///
    /// # Arguments
    ///
    /// * `group_id` - The ID of the group to update.
    /// * `new_group` - The new `ConflictGroup` to use for task creation.
    /// * `priority` - The priority to assign to the new tasks.
    fn update_tasks(
        &mut self,
        group_id: GroupId,
        new_group: &ConflictGroup,
        priority: TaskPriority,
    ) {
        trace!(
            group = group_id,
            priority = priority.display(),
            "Updating tasks",
        );
        // Cancel existing tasks for this grou
        self.cancel_tasks_for_group(group_id);

        // Create new tasks for the updated group
        self.create_new_tasks(new_group, priority);
    }

    /// Cancels all tasks for a given group
    fn cancel_tasks_for_group(&mut self, group_id: GroupId) {
        let temp_queue = SegQueue::new();

        while let Some(task) = self.task_queue.pop() {
            if task.group_idx != group_id {
                temp_queue.push(task);
            }
        }

        while let Some(task) = temp_queue.pop() {
            self.task_queue.push(task);
        }
    }

    /// Creates and spawns new tasks for a given order group.
    ///
    /// # Arguments
    ///
    /// * `new_group` - The `ConflictGroup` to create tasks for.
    /// * `priority` - The priority to assign to the tasks.
    fn create_new_tasks(&mut self, new_group: &ConflictGroup, priority: TaskPriority) {
        let tasks = get_tasks_for_group(new_group, priority, self.safe_sorting_only);
        for task in tasks {
            self.task_queue.push(task);
        }
    }
}

/// Generates a vector of conflict tasks for a given order group.
///
/// # Arguments
///
/// * `group` - The `ConflictGroup` to create tasks for.
/// * `priority` - The priority to assign to the tasks.
/// * `safe_sorting_only` - See [ParallelBuilderConfig::safe_sorting_only]
///
/// # Returns
///
/// A vector of `ConflictTask`s for the given group.
pub fn get_tasks_for_group(
    group: &ConflictGroup,
    priority: TaskPriority,
    safe_sorting_only: bool,
) -> Vec<ConflictTask> {
    let mut tasks = vec![];

    let created_at = Instant::now();
    // We want to run Greedy first so we can get quick, decent results
    tasks.push(ConflictTask {
        group_idx: group.id,
        algorithm: Algorithm::Greedy,
        priority,
        group: group.clone(),
        created_at,
    });

    // Then, we can push lower priority tasks that have a low chance, but a chance, of finding a better result
    if group.orders.len() <= MAX_LENGTH_FOR_ALL_PERMUTATIONS && !safe_sorting_only {
        // AllPermutations
        tasks.push(ConflictTask {
            group_idx: group.id,
            algorithm: Algorithm::AllPermutations,
            priority,
            group: group.clone(),
            created_at,
        });
    } else {
        if !safe_sorting_only {
            // Random
            tasks.push(ConflictTask {
                group_idx: group.id,
                algorithm: Algorithm::Random {
                    seed: group.id as u64,
                    count: NUMBER_OF_RANDOM_TASKS,
                },
                priority: TaskPriority::Low,
                group: group.clone(),
                created_at,
            });
        }
        tasks.push(ConflictTask {
            group_idx: group.id,
            algorithm: Algorithm::Length,
            priority: TaskPriority::Low,
            group: group.clone(),
            created_at,
        });
        if !safe_sorting_only {
            tasks.push(ConflictTask {
                group_idx: group.id,
                algorithm: Algorithm::ReverseGreedy,
                priority: TaskPriority::Low,
                group: group.clone(),
                created_at,
            });
        }
    }

    tasks
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::TxLegacy;
    use alloy_primitives::{Address, TxHash, B256, U256};
    use rbuilder_primitives::{
        evm_inspector::{SlotKey, UsedStateTrace},
        MempoolTx, Order, SimValue, SimulatedOrder, TransactionSignedEcRecoveredWithBlobs,
    };
    use reth::primitives::{Transaction, TransactionSigned};
    use reth_primitives::Recovered;
    use std::sync::Arc;

    use std::sync::mpsc;

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
            coinbase_profit: U256,
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

            let sim_value = SimValue::new_test_no_gas(coinbase_profit, U256::ZERO);

            Arc::new(SimulatedOrder {
                order: Order::Tx(MempoolTx {
                    tx_with_blobs: TransactionSignedEcRecoveredWithBlobs::new_no_blobs(
                        self.create_tx(),
                    )
                    .unwrap(),
                }),
                sim_value,
                used_state_trace: Some(trace),
            })
        }
    }

    // Helper function to create an order group
    fn create_conflict_group(
        id: GroupId,
        orders: Vec<Arc<SimulatedOrder>>,
        conflicting_ids: HashSet<GroupId>,
    ) -> ConflictGroup {
        ConflictGroup {
            id,
            orders: Arc::new(orders),
            conflicting_group_ids: Arc::new(conflicting_ids.into_iter().collect()),
        }
    }

    fn create_task_queue() -> Arc<SegQueue<ConflictTask>> {
        Arc::new(SegQueue::new())
    }

    fn create_task_generator() -> ConflictTaskGenerator {
        let (sender, _receiver) = mpsc::channel();
        ConflictTaskGenerator::new(false, create_task_queue(), sender)
    }

    #[test]
    fn test_process_single_group_new() {
        let mut conflict_manager = create_task_generator();

        let mut data_generator = DataGenerator::new();
        let group = create_conflict_group(
            1,
            vec![
                data_generator.create_order(None, None, U256::from(100)),
                data_generator.create_order(None, None, U256::from(200)),
            ],
            HashSet::default(),
        );
        conflict_manager.process_single_group(group.clone());

        assert_eq!(conflict_manager.existing_groups.len(), 1);
        assert!(conflict_manager.existing_groups.contains_key(&1));
        assert_eq!(conflict_manager.task_queue.len(), 2);
    }

    #[test]
    fn test_process_single_group_update() {
        let mut conflict_manager = create_task_generator();

        let mut data_generator = DataGenerator::new();

        let initial_group = create_conflict_group(
            1,
            vec![
                data_generator.create_order(None, None, U256::from(100)),
                data_generator.create_order(None, None, U256::from(200)),
            ],
            HashSet::default(),
        );
        conflict_manager.existing_groups.insert(1, initial_group);

        let updated_group = create_conflict_group(
            1,
            vec![
                data_generator.create_order(None, None, U256::from(200)),
                data_generator.create_order(None, None, U256::from(300)),
            ],
            HashSet::default(),
        );
        conflict_manager.process_single_group(updated_group);

        assert_eq!(conflict_manager.existing_groups.len(), 1);
        assert_eq!(conflict_manager.task_queue.len(), 2);
    }

    #[test]
    fn test_conflicting_groups_remove_tasks() {
        let mut conflict_manager = create_task_generator();

        let mut data_generator = DataGenerator::new();

        let mut group2_conflicts_with_group1 = HashSet::default();
        group2_conflicts_with_group1.insert(1_usize);

        let group1 = create_conflict_group(
            1,
            vec![
                data_generator.create_order(None, None, U256::from(100)),
                data_generator.create_order(None, None, U256::from(200)),
            ],
            HashSet::default(),
        );
        let group2 = create_conflict_group(
            2,
            vec![
                data_generator.create_order(None, None, U256::from(200)),
                data_generator.create_order(None, None, U256::from(300)),
            ],
            group2_conflicts_with_group1,
        );

        conflict_manager.process_groups(vec![group1, group2]);

        assert_eq!(conflict_manager.existing_groups.len(), 1);
        assert!(conflict_manager.existing_groups.contains_key(&2));
        assert_eq!(conflict_manager.task_queue.len(), 2);
    }

    #[test]
    fn test_process_groups() {
        let mut conflict_manager = create_task_generator();

        let mut data_generator = DataGenerator::new();

        let mut group2_conflicts_with_group1 = HashSet::default();
        group2_conflicts_with_group1.insert(1_usize);

        let groups = vec![
            create_conflict_group(
                1,
                vec![
                    data_generator.create_order(None, None, U256::from(100)),
                    data_generator.create_order(None, None, U256::from(200)),
                ],
                HashSet::default(),
            ),
            create_conflict_group(
                2,
                vec![
                    data_generator.create_order(None, None, U256::from(200)),
                    data_generator.create_order(None, None, U256::from(300)),
                ],
                group2_conflicts_with_group1,
            ),
            create_conflict_group(
                3,
                vec![
                    data_generator.create_order(None, None, U256::from(300)),
                    data_generator.create_order(None, None, U256::from(400)),
                ],
                HashSet::default(),
            ),
        ];

        conflict_manager.process_groups(groups);

        assert_eq!(conflict_manager.existing_groups.len(), 2);
        assert!(conflict_manager.existing_groups.contains_key(&2));
        assert!(conflict_manager.existing_groups.contains_key(&3));
        assert_eq!(conflict_manager.task_queue.len(), 4);
    }

    #[test]
    fn test_has_significant_changes() {
        let conflict_manager = create_task_generator();

        let mut data_generator = DataGenerator::new();

        let group1 = create_conflict_group(
            1,
            vec![
                data_generator.create_order(None, None, U256::from(100)),
                data_generator.create_order(None, None, U256::from(90)),
                data_generator.create_order(None, None, U256::from(80)),
            ],
            HashSet::default(),
        );

        let group2 = create_conflict_group(
            1,
            vec![
                data_generator.create_order(None, None, U256::from(150)),
                data_generator.create_order(None, None, U256::from(140)),
                data_generator.create_order(None, None, U256::from(130)),
            ],
            HashSet::default(),
        );

        assert!(conflict_manager.has_significant_changes(&group2, &group1));

        let group3 = create_conflict_group(
            1,
            vec![
                data_generator.create_order(None, None, U256::from(110)),
                data_generator.create_order(None, None, U256::from(100)),
                data_generator.create_order(None, None, U256::from(90)),
            ],
            HashSet::default(),
        );

        assert!(!conflict_manager.has_significant_changes(&group3, &group1));
    }

    #[test]
    fn has_group_changed() {
        let conflict_manager = create_task_generator();

        let mut data_generator = DataGenerator::new();

        let order1 = data_generator.create_order(None, None, U256::from(100));

        let group1 = create_conflict_group(1, vec![order1.clone()], HashSet::default());

        let group2 = create_conflict_group(
            1,
            vec![data_generator.create_order(None, None, U256::from(200))],
            HashSet::default(),
        );

        assert!(conflict_manager.group_has_changed(&group2, &group1));

        let group3 = create_conflict_group(1, vec![order1], HashSet::default());

        assert!(!conflict_manager.group_has_changed(&group3, &group1));
    }

    #[test]
    fn test_single_order_group() {
        let mut conflict_manager = create_task_generator();

        let mut data_generator = DataGenerator::new();

        let group = create_conflict_group(
            1,
            vec![data_generator.create_order(None, None, U256::from(100))],
            HashSet::default(),
        );

        conflict_manager.process_single_group(group);

        assert_eq!(conflict_manager.existing_groups.len(), 1);
        assert!(conflict_manager.existing_groups.contains_key(&1));

        // Should not create new tasks
        assert_eq!(conflict_manager.task_queue.len(), 0);
    }
}
