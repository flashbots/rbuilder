use super::{ConflictGroup, GroupId, ResolutionResult};
use alloy_primitives::{utils::format_ether, U256};
use dashmap::DashMap;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc as std_mpsc, Arc,
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::trace;

pub struct BestResults {
    pub data: DashMap<GroupId, (ResolutionResult, ConflictGroup)>,
    pub version: AtomicU64,
}

impl BestResults {
    pub fn new() -> Self {
        Self {
            data: DashMap::new(),
            version: AtomicU64::new(0),
        }
    }

    pub fn increment_version(&self) {
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    pub fn get_results_and_version(&self) -> (Vec<(ResolutionResult, ConflictGroup)>, u64) {
        let results = self
            .data
            .iter()
            .map(|entry| {
                let (_, (sequence_of_orders, order_group)) = entry.pair();
                (sequence_of_orders.clone(), order_group.clone())
            })
            .collect();
        (results, self.version.load(Ordering::SeqCst))
    }

    pub fn get_number_of_orders(&self) -> usize {
        self.data
            .iter()
            .map(|entry| entry.value().1.orders.len())
            .sum()
    }

    pub fn get_version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }
}

impl Default for BestResults {
    fn default() -> Self {
        Self::new()
    }
}

/// Collects and manages the best results for each group in a concurrent environment.
///
/// This struct is responsible for receiving group results, updating the best known
/// results for each group, and triggering builds when better results are found.
pub struct ResultsAggregator {
    group_result_receiver: std_mpsc::Receiver<(GroupId, (ResolutionResult, ConflictGroup))>,
    best_results: Arc<BestResults>,
}

impl ResultsAggregator {
    /// Creates a new `ResultsAggregator` instance.
    ///
    /// # Arguments
    ///
    /// * `group_result_receiver` - A receiver for incoming group results.
    /// * `best_results` - A shared, thread-safe map to store the best results for each group.
    /// * `build_trigger` - A sender to trigger builds when better results are found.
    pub fn new(
        group_result_receiver: std_mpsc::Receiver<(GroupId, (ResolutionResult, ConflictGroup))>,
        best_results: Arc<BestResults>,
    ) -> Self {
        Self {
            group_result_receiver,
            best_results,
        }
    }

    /// Updates the best result for a given group.
    ///
    /// # Arguments
    ///
    /// * `group_id` - The ID of the group to update.
    /// * `sequence_of_orders` - The new ordering information for the group.
    /// * `order_group` - The new order group.
    ///
    /// # Returns
    ///
    /// Returns a tuple where:
    /// - The first element is a boolean indicating if the best result was updated.
    /// - The second element is the old profit before the update.
    fn update_best_result(
        &self,
        group_id: GroupId,
        sequence_of_orders: ResolutionResult,
        order_group: ConflictGroup,
    ) -> (bool, U256) {
        let mut best_result_updated = false;
        let mut old_profit = U256::ZERO;

        // First, check for conflicting groups that need to be removed
        let mut conflicting_groups_to_remove = Vec::new();
        for &conflicting_id in &*order_group.conflicting_group_ids {
            if let Some(existing_entry) = self.best_results.data.get(&conflicting_id) {
                if sequence_of_orders.total_profit > existing_entry.value().0.total_profit {
                    conflicting_groups_to_remove.push(conflicting_id);
                } else {
                    // If the new group is not more profitable than an existing conflicting group,
                    // we don't update anything
                    return (false, old_profit);
                }
            }
        }

        // Insert or update the new group
        {
            let mut entry = self.best_results.data.entry(group_id).or_insert_with(|| {
                best_result_updated = true;
                (sequence_of_orders.clone(), order_group.clone())
            });

            if sequence_of_orders.total_profit > entry.value().0.total_profit {
                old_profit = entry.value().0.total_profit;
                *entry.value_mut() = (sequence_of_orders, order_group);
                best_result_updated = true;
            }
        }

        // Remove all conflicting groups
        for conflicting_id in &conflicting_groups_to_remove {
            self.best_results.data.remove(conflicting_id);
        }

        // If any updates were made, increment the version
        if best_result_updated || !conflicting_groups_to_remove.is_empty() {
            self.best_results.increment_version();
        }

        (best_result_updated, old_profit)
    }

    /// Runs the results collection process.
    ///
    /// This method continuously receives group results, updates the best results,
    /// and triggers builds when necessary. It runs until the receiver is closed.
    pub async fn run(&mut self, cancel_token: CancellationToken) {
        loop {
            if cancel_token.is_cancelled() {
                // Cancellation requested, exit the loop
                break;
            }

            match self.group_result_receiver.try_recv() {
                Ok((group_id, (sequence_of_orders, order_group))) => {
                    self.process_update(group_id, sequence_of_orders, order_group);
                }
                Err(std_mpsc::TryRecvError::Empty) => {
                    // No message available, yield to other tasks
                    tokio::task::yield_now().await;
                }
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    // Channel has been closed, exit the loop
                    break;
                }
            }
        }
    }

    /// Processes an update to the best results for a given group.
    ///
    /// # Arguments
    ///
    /// * `group_id` - The ID of the group to update.
    /// * `sequence_of_orders` - The new ordering information for the group.
    /// * `order_group` - The new order group.
    fn process_update(
        &self,
        group_id: GroupId,
        sequence_of_orders: ResolutionResult,
        order_group: ConflictGroup,
    ) {
        let start = Instant::now();
        trace!("Received group result for group: {:?}", group_id);
        let (best_result_updated, old_profit) =
            self.update_best_result(group_id, sequence_of_orders.clone(), order_group);
        let duration = start.elapsed();

        if best_result_updated {
            self.log_updated_result(group_id, &sequence_of_orders, old_profit, duration);
        } else {
            trace!(
                "Best result not updated for group: {:?}, duration: {:?}",
                group_id,
                duration
            );
        }
    }

    /// Helper function to log the updated best result for a given group.
    fn log_updated_result(
        &self,
        group_id: GroupId,
        sequence_of_orders: &ResolutionResult,
        old_profit: U256,
        duration: Duration,
    ) {
        trace!(
            group_id = group_id,
            old_profit = format_ether(old_profit),
            new_profit = format_ether(sequence_of_orders.total_profit),
            sum_of_best_results = format_ether(self.get_and_display_sum_of_best_results()),
            number_of_orders = self.best_results.get_number_of_orders(),
            version = self.best_results.get_version(),
            duration_ns = duration.as_nanos(),
            "Updated best result for group"
        );
    }

    /// Returns the sum of the total profits of all the best results.
    /// Helper function that is used for traceging.
    fn get_and_display_sum_of_best_results(&self) -> U256 {
        self.best_results
            .data
            .iter()
            .map(|entry| entry.value().0.total_profit)
            .sum::<U256>()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;

    use super::*;
    use std::sync::Arc;

    // Helper function to create a simple ConflictGroup
    fn create_order_group(id: usize, conflicting_ids: Vec<usize>) -> ConflictGroup {
        ConflictGroup {
            id,
            orders: Arc::new(vec![]),
            conflicting_group_ids: Arc::new(conflicting_ids.into_iter().collect()),
        }
    }

    // Helper function to create a simple ResolutionResult
    fn create_sequence_of_orders(profit: u64) -> ResolutionResult {
        ResolutionResult {
            total_profit: U256::from(profit),
            sequence_of_orders: vec![],
        }
    }

    fn create_results_aggregator(best_results: Option<Arc<BestResults>>) -> Arc<ResultsAggregator> {
        let best_results = best_results.unwrap_or_else(|| Arc::new(BestResults::new()));
        Arc::new(ResultsAggregator::new(
            std_mpsc::channel().1,
            best_results,
            // mpsc::channel(1).0,
        ))
    }

    #[test]
    fn test_update_best_result_new_entry() {
        let results_aggregator = create_results_aggregator(None);

        let group_id = 1;
        let sequence_of_orders = create_sequence_of_orders(100);
        let order_group = create_order_group(group_id, vec![]);

        let (best_result_updated, old_profit) = results_aggregator.update_best_result(
            group_id,
            sequence_of_orders.clone(),
            order_group.clone(),
        );
        assert!(best_result_updated);
        assert_eq!(old_profit, U256::ZERO);

        let best_result = results_aggregator.best_results.data.get(&group_id).unwrap();
        assert_eq!(best_result.value().0.total_profit, U256::from(100));
        assert_eq!(best_result.value().1.id, group_id);
    }

    #[test]
    fn test_update_best_result_better_profit() {
        let results_aggregator = create_results_aggregator(None);

        let group_id = 1;
        let initial_sequence_of_orders = create_sequence_of_orders(100);
        let initial_order_group = create_order_group(group_id, vec![]);
        let (best_result_updated, _) = results_aggregator.update_best_result(
            group_id,
            initial_sequence_of_orders,
            initial_order_group,
        );
        assert!(best_result_updated);

        let better_sequence_of_orders = create_sequence_of_orders(200);
        let better_order_group = create_order_group(group_id, vec![]);

        let (best_result_updated, old_profit) = results_aggregator.update_best_result(
            group_id,
            better_sequence_of_orders.clone(),
            better_order_group.clone(),
        );
        assert!(best_result_updated);
        assert_eq!(old_profit, U256::from(100));

        let best_result = results_aggregator.best_results.data.get(&group_id).unwrap();
        assert_eq!(best_result.value().0.total_profit, U256::from(200));
    }

    #[test]
    fn test_update_best_result_worse_profit() {
        let results_aggregator = create_results_aggregator(None);

        let group_id = 1;
        let initial_sequence_of_orders = create_sequence_of_orders(200);
        let initial_order_group = create_order_group(group_id, vec![]);
        results_aggregator
            .best_results
            .data
            .insert(group_id, (initial_sequence_of_orders, initial_order_group));

        let worse_sequence_of_orders = create_sequence_of_orders(100);
        let worse_order_group = create_order_group(group_id, vec![]);

        let (best_result_updated, _) = results_aggregator.update_best_result(
            group_id,
            worse_sequence_of_orders.clone(),
            worse_order_group.clone(),
        );
        assert!(!best_result_updated);

        let best_result = results_aggregator.best_results.data.get(&group_id).unwrap();
        assert_eq!(best_result.value().0.total_profit, U256::from(200));
    }

    #[test]
    fn test_update_best_result_remove_conflicting() {
        let results_aggregator = create_results_aggregator(None);

        let group_id_1 = 1;
        let group_id_2 = 2;

        let sequence_of_orders_1 = create_sequence_of_orders(100);
        let order_group_1 = create_order_group(group_id_1, vec![]);

        // Group 2 is WORSE than group 1
        let sequence_of_orders_2 = create_sequence_of_orders(150);
        let order_group_2 = create_order_group(group_id_2, vec![group_id_1]);

        let (best_result_updated, _) = results_aggregator.update_best_result(
            group_id_1,
            sequence_of_orders_1.clone(),
            order_group_1.clone(),
        );
        assert!(best_result_updated);

        let (best_result_updated, _) = results_aggregator.update_best_result(
            group_id_2,
            sequence_of_orders_2.clone(),
            order_group_2.clone(),
        );
        assert!(best_result_updated);

        // Group 1 is removed because group 2 is better
        assert!(!results_aggregator
            .best_results
            .data
            .contains_key(&group_id_1));

        // Group 2 is kept because it is better
        assert!(results_aggregator
            .best_results
            .data
            .contains_key(&group_id_2));

        // Group 2 is the best result
        let best_result = results_aggregator
            .best_results
            .data
            .get(&group_id_2)
            .unwrap();
        assert_eq!(best_result.value().0.total_profit, U256::from(150));

        // Group 1 is not in the best results
        let best_result_1 = results_aggregator.best_results.data.get(&group_id_1);
        assert!(best_result_1.is_none());
    }

    #[test]
    fn test_update_best_result_keep_better_conflicting() {
        let results_aggregator = create_results_aggregator(None);

        let group_id_1 = 1;
        let group_id_2 = 2;
        let sequence_of_orders_1 = create_sequence_of_orders(100);
        let order_group_1 = create_order_group(group_id_1, vec![]);

        // Group 2 is worse than group 1
        let sequence_of_orders_2 = create_sequence_of_orders(50);
        let order_group_2 = create_order_group(group_id_2, vec![group_id_1]);

        assert!(
            results_aggregator
                .update_best_result(
                    group_id_1,
                    sequence_of_orders_1.clone(),
                    order_group_1.clone()
                )
                .0
        );
        assert!(
            !results_aggregator
                .update_best_result(group_id_2, sequence_of_orders_2, order_group_2)
                .0
        );

        assert!(results_aggregator
            .best_results
            .data
            .contains_key(&group_id_1));
        assert!(!results_aggregator
            .best_results
            .data
            .contains_key(&group_id_2));
        assert_eq!(
            results_aggregator
                .best_results
                .data
                .get(&group_id_1)
                .unwrap()
                .value()
                .0
                .total_profit,
            U256::from(100)
        );
        assert!(results_aggregator
            .best_results
            .data
            .get(&group_id_2)
            .is_none());
    }

    #[test]
    fn test_update_best_result_merge_conflicting_groups() {
        let results_aggregator = create_results_aggregator(None);

        let group_id_1 = 1;
        let group_id_2 = 2;
        let group_id_3 = 3;

        // Create two initial groups
        let sequence_of_orders_1 = create_sequence_of_orders(100);
        let order_group_1 = create_order_group(group_id_1, vec![]);

        let sequence_of_orders_2 = create_sequence_of_orders(150);
        let order_group_2 = create_order_group(group_id_2, vec![]);

        // Add the initial groups
        assert!(
            results_aggregator
                .update_best_result(group_id_1, sequence_of_orders_1, order_group_1)
                .0
        );
        assert!(
            results_aggregator
                .update_best_result(group_id_2, sequence_of_orders_2, order_group_2)
                .0
        );

        // Create a third group that conflicts with both group 1 and 2, but is more profitable
        let sequence_of_orders_3 = create_sequence_of_orders(300);
        let order_group_3 = create_order_group(group_id_3, vec![group_id_1, group_id_2]);

        // Update with the new, more profitable group
        assert!(
            results_aggregator
                .update_best_result(group_id_3, sequence_of_orders_3, order_group_3)
                .0
        );

        // Check that group 3 is now the only group in best_results
        assert_eq!(results_aggregator.best_results.data.len(), 1);
        assert!(results_aggregator
            .best_results
            .data
            .contains_key(&group_id_3));
        assert!(!results_aggregator
            .best_results
            .data
            .contains_key(&group_id_1));
        assert!(!results_aggregator
            .best_results
            .data
            .contains_key(&group_id_2));

        // Verify the profit of the remaining group
        let best_result = results_aggregator
            .best_results
            .data
            .get(&group_id_3)
            .unwrap();
        assert_eq!(best_result.value().0.total_profit, U256::from(300));
    }

    #[test]
    fn test_update_best_result_version_increment() {
        let best_results = Arc::new(BestResults::new());
        let results_aggregator = create_results_aggregator(Some(Arc::clone(&best_results)));

        let initial_version = best_results.get_version();

        // Update with a new group
        let group_id = 1;
        let sequence_of_orders = create_sequence_of_orders(100);
        let order_group = create_order_group(group_id, vec![]);
        let (updated, _) =
            results_aggregator.update_best_result(group_id, sequence_of_orders, order_group);
        assert!(updated);

        // Version should have incremented
        let new_version = best_results.get_version();
        assert_eq!(new_version, initial_version + 1);
    }
}
