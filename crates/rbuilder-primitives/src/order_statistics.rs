use crate::Order;
use std::ops::{Add, Sub};

/// Simple struct to count orders by type.
#[derive(Clone, Debug, Default)]
pub struct OrderStatistics {
    tx_count: i32,
    bundle_count: i32,
    sbundle_count: i32,
}

impl OrderStatistics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, order: &Order) {
        match order {
            Order::Bundle(_) => self.bundle_count += 1,
            Order::Tx(_) => self.tx_count += 1,
            Order::ShareBundle(_) => self.sbundle_count += 1,
        }
    }

    pub fn remove(&mut self, order: &Order) {
        match order {
            Order::Bundle(_) => self.bundle_count -= 1,
            Order::Tx(_) => self.tx_count -= 1,
            Order::ShareBundle(_) => self.sbundle_count -= 1,
        }
    }

    pub fn total(&self) -> u64 {
        self.tx_count as u64 + self.bundle_count as u64 + self.sbundle_count as u64
    }
}

impl Add for OrderStatistics {
    type Output = Self;

    fn add(self, other: Self) -> Self::Output {
        Self {
            tx_count: self.tx_count + other.tx_count,
            bundle_count: self.bundle_count + other.bundle_count,
            sbundle_count: self.sbundle_count + other.sbundle_count,
        }
    }
}

impl Sub for OrderStatistics {
    type Output = Self;

    fn sub(self, other: Self) -> Self::Output {
        Self {
            tx_count: self.tx_count - other.tx_count,
            bundle_count: self.bundle_count - other.bundle_count,
            sbundle_count: self.sbundle_count - other.sbundle_count,
        }
    }
}
