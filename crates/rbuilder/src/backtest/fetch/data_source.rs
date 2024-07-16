use crate::backtest::{BuiltBlockData, OrdersWithTimestamp};
use alloy_primitives::B256;
use async_trait::async_trait;

/// DataSource trait
///
/// This trait is used to fetch data from a datasource
#[async_trait]
pub trait DataSource: std::fmt::Debug {
    async fn get_orders(&self, block: BlockRef) -> eyre::Result<Vec<OrdersWithTimestamp>>;

    async fn get_built_block_data(&self, block_hash: B256) -> eyre::Result<Option<BuiltBlockData>>;

    fn clone_box(&self) -> Box<dyn DataSource>;
}

impl Clone for Box<dyn DataSource> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// Some DataSources need also the block_timestamp to be able to get the orders
/// so we use a BlockRef on [`DataSource::get_orders`] instead of just a block_number
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
