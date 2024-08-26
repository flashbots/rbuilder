use crate::backtest::{BuiltBlockData, OrdersWithTimestamp};
use alloy_primitives::B256;
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct DatasourceData {
    pub orders: Vec<OrdersWithTimestamp>,
    pub built_block_data: Option<BuiltBlockData>,
}

/// DataSource trait
///
/// This trait is used to fetch data from a datasource
#[async_trait]
pub trait DataSource: std::fmt::Debug {
    async fn get_data(&self, block: BlockRef) -> eyre::Result<DatasourceData>;

    fn clone_box(&self) -> Box<dyn DataSource>;
}

impl Clone for Box<dyn DataSource> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// Some DataSources need also the block_timestamp and landed_block_hash to be able to get the orders
/// so we use a BlockRef on [`DataSource::get_orders`] instead of just a block_number
#[derive(Debug, Copy, Clone)]
pub struct BlockRef {
    pub block_number: u64,
    pub block_timestamp: u64,
    pub landed_block_hash: Option<B256>,
}

impl BlockRef {
    pub fn new(block_number: u64, block_timestamp: u64, landed_block_hash: Option<B256>) -> Self {
        Self {
            block_number,
            block_timestamp,
            landed_block_hash,
        }
    }
}
