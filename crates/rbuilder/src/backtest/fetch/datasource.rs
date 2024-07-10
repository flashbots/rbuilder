use crate::backtest::OrdersWithTimestamp;
use async_trait::async_trait;

/// DataSource trait
///
/// This trait is used to fetch data from a datasource
#[async_trait]
pub trait DataSource: std::fmt::Debug {
    async fn get_orders(&self, block: BlockRef) -> eyre::Result<Vec<OrdersWithTimestamp>>;
    fn clone_box(&self) -> Box<dyn DataSource>;
}

impl Clone for Box<dyn DataSource> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

#[derive(Debug, Copy, Clone)]
pub struct BlockRef {
    pub block_number: u64,
    pub block_timestamp: u64,
}

impl BlockRef {
    pub fn new(block_number: u64, block_timestamp: u64) -> Self {
        Self {
            block_number,
            block_timestamp,
        }
    }
}
