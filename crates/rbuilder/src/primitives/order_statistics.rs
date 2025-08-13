use crate::primitives::Order;

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
}
