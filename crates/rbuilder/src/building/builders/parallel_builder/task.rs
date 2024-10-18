use std::{cmp::Ordering, time::Instant};

use super::ConflictGroup;

/// ConflictTask provides a task for resolving a [ConflictGroup] with a specific [Algorithm].
#[derive(Debug, Clone)]
pub struct ConflictTask {
    pub group_idx: usize,
    pub algorithm: Algorithm,
    pub priority: TaskPriority,
    pub group: ConflictGroup,
    pub created_at: Instant,
}

/// TaskPriority provides a priority for a [ConflictTask].
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
pub enum TaskPriority {
    Low = 0,
    Medium = 1,
    High = 2,
}

impl TaskPriority {
    pub fn display(&self) -> &str {
        match self {
            TaskPriority::Low => "Low",
            TaskPriority::Medium => "Medium",
            TaskPriority::High => "High",
        }
    }
}

/// [PartialEq] [Eq] [PartialOrd] [Ord] are the traits that are required for a [ConflictTask] to be used in a priority queue.
impl PartialEq for ConflictTask {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority
    }
}

impl Eq for ConflictTask {}

impl PartialOrd for ConflictTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ConflictTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority first, then earlier created_at
        other
            .priority
            .cmp(&self.priority)
            .then_with(|| self.created_at.cmp(&other.created_at))
    }
}

/// Algorithm provides an algorithm for resolving a [ConflictGroup].
/// Initially these are all algorithms that produce a sequence of orders to execute.
#[derive(Debug, Clone, Copy)]
pub enum Algorithm {
    /// `Greedy` checks the following ordrerings: max profit, mev gas price
    Greedy,
    /// `ReverseGreedy` checks the reverse greedy orderings: e.g. min profit, min mev gas price first
    ReverseGreedy,
    /// `Length` checks the length based orderings
    Length,
    /// `AllPermutations` checks all possible permutations of the group.
    AllPermutations,
    /// `Random` checks random permutations of the group.
    Random { seed: u64, count: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_priority_ordering() {
        assert!(TaskPriority::Low < TaskPriority::Medium);
        assert!(TaskPriority::Medium < TaskPriority::High);
        assert!(TaskPriority::Low < TaskPriority::High);
    }

    #[test]
    fn test_task_priority_display() {
        assert_eq!(TaskPriority::Low.display(), "Low");
        assert_eq!(TaskPriority::Medium.display(), "Medium");
        assert_eq!(TaskPriority::High.display(), "High");
    }

    #[test]
    fn test_task_priority_equality() {
        assert_eq!(TaskPriority::Low, TaskPriority::Low);
        assert_ne!(TaskPriority::Low, TaskPriority::Medium);
        assert_ne!(TaskPriority::Low, TaskPriority::High);
    }

    // to-do: test equal priority ordering by created_at
}
