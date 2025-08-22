use std::{collections::HashMap, sync::Arc};

use ahash::HashSet;
use parking_lot::Mutex;

use crate::{
    building::builders::{
        block_building_helper::BiddableUnfinishedBlock, UnfinishedBlockBuildingSink,
    },
    primitives::SimulatedOrder,
};

/// Wrapper to make SimulatedOrder hashable to use in HashSet.
#[derive(Debug)]
struct HashedSimulatedOrder(Arc<SimulatedOrder>);

impl std::hash::Hash for HashedSimulatedOrder {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::sync::Arc::<SimulatedOrder>::as_ptr(&self.0).hash(state)
    }
}

impl PartialEq for HashedSimulatedOrder {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for HashedSimulatedOrder {}

/// For this first version we only cache the set (so it's faster for searching) of orders.
#[derive(Debug)]
pub struct BuiltBlockInfo {
    sim_orders: HashSet<HashedSimulatedOrder>,
}

impl BuiltBlockInfo {
    pub fn new() -> Self {
        Self {
            sim_orders: HashSet::default(),
        }
    }

    pub fn add_sim_order(&mut self, sim_order: Arc<SimulatedOrder>) {
        self.sim_orders.insert(HashedSimulatedOrder(sim_order));
    }

    pub fn contains_sim_order(&self, sim_order: &Arc<SimulatedOrder>) -> bool {
        self.sim_orders
            .contains(&HashedSimulatedOrder(sim_order.clone()))
    }
}

/// A cache of built blocks so BlockBuildingAlgorithm can recycle information
#[derive(Debug)]
pub struct BuiltBlockCache {
    /// key is the builder name
    blocks_infos: Mutex<HashMap<String, Arc<BuiltBlockInfo>>>,
}

impl BuiltBlockCache {
    pub fn new() -> Self {
        Self {
            blocks_infos: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_new_block(&self, builder_name: String, block: Arc<BuiltBlockInfo>) {
        self.blocks_infos.lock().insert(builder_name, block);
    }

    /// Returns a list of all blocks that are not from the builder with the given name.
    pub fn get_block_infos(&self, filter_out_builder_name: &str) -> Vec<Arc<BuiltBlockInfo>> {
        let blocks_infos = self.blocks_infos.lock();
        blocks_infos
            .iter()
            .filter(|(builder_name, _)| *builder_name != filter_out_builder_name)
            .map(|(_, block)| block.clone())
            .collect()
    }
}

/// Simple wrapper to update the built block cache when a new block is built.
#[derive(Debug)]
pub struct BuiltBlockCacheUpdater {
    built_block_cache: Arc<BuiltBlockCache>,
    sink: Arc<dyn UnfinishedBlockBuildingSink>,
}

impl BuiltBlockCacheUpdater {
    pub fn new(
        built_block_cache: Arc<BuiltBlockCache>,
        sink: Arc<dyn UnfinishedBlockBuildingSink>,
    ) -> Self {
        Self {
            built_block_cache,
            sink,
        }
    }
}

impl UnfinishedBlockBuildingSink for BuiltBlockCacheUpdater {
    fn new_block(&self, block: BiddableUnfinishedBlock) {
        let mut block_info = BuiltBlockInfo::new();
        for execution_result in &block.block().built_block_trace().included_orders {
            block_info.add_sim_order(execution_result.sim_order.clone());
        }
        self.built_block_cache.set_new_block(
            block.block().builder_name().to_string(),
            Arc::new(block_info),
        );
        self.sink.new_block(block);
    }

    fn can_use_suggested_fee_recipient_as_coinbase(&self) -> bool {
        self.sink.can_use_suggested_fee_recipient_as_coinbase()
    }
}
