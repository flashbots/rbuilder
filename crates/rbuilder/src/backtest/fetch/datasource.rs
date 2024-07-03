use crate::backtest::OrdersWithTimestamp;
use async_trait::async_trait;

/// Datasource trait
///
/// This trait is used to fetch data from a datasource
#[async_trait]
pub trait Datasource: std::fmt::Debug {
    async fn get_data(&self, block: BlockRef) -> eyre::Result<Vec<OrdersWithTimestamp>>;
    fn clone_box(&self) -> Box<dyn Datasource>;
}

impl Clone for Box<dyn Datasource> {
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
