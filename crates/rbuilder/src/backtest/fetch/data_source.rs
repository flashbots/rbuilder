use crate::{
    backtest::{
        full_slot_block_data::ReplaceableOrderPoolCommandWithTimestamp, BuiltBlockData,
        OrdersWithTimestamp,
    },
    live_builder::order_input::ReplaceableOrderPoolCommand,
};
use alloy_primitives::B256;
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct DatasourceData {
    /// BE CAREFUL: Depending on the source/order-type orders might be preprocessed for uuid replacements (you may have several orders with the same replacement id)
    pub orders: Vec<OrdersWithTimestamp>,
    pub built_block_data: Option<BuiltBlockData>,
}

#[derive(Debug, Clone)]
pub struct FullSlotDatasourceData {
    /// Orders are NOT guarantied to be sorted by timestamp.
    pub orders: Vec<ReplaceableOrderPoolCommandWithTimestamp>,
    pub built_block_data: Option<BuiltBlockData>,
}

/// DataSource trait
///
/// This trait is used to fetch data from a datasource
#[async_trait]
pub trait DataSource: std::fmt::Debug {
    async fn get_data(&self, block: BlockRef) -> eyre::Result<DatasourceData>;
    async fn get_full_slot_data(&self, block: BlockRef) -> eyre::Result<FullSlotDatasourceData>;

    fn clone_box(&self) -> Box<dyn DataSource>;
}

/// Helper for sources that already implemented get_data and have no replacements.
pub async fn get_full_slot_data_from_data(
    source: &impl DataSource,
    block: BlockRef,
) -> eyre::Result<FullSlotDatasourceData> {
    let data = source.get_data(block).await?;
    Ok(FullSlotDatasourceData {
        orders: data
            .orders
            .into_iter()
            .map(|o| ReplaceableOrderPoolCommandWithTimestamp {
                timestamp_ms: o.timestamp_ms,
                command: ReplaceableOrderPoolCommand::Order(o.order),
            })
            .collect(),
        built_block_data: data.built_block_data,
    })
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
