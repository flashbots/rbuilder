use std::{collections::HashMap, sync::Arc};

use ahash::HashSet;
use parking_lot::Mutex;

use crate::building::builders::block_building_helper::BlockBuildingHelper;
use rbuilder_primitives::{Order, OrderId};

/// For this first version we only cache the set (so it's faster for searching) of orders.
#[derive(Debug)]
pub struct BuiltBlockInfo {
    orders_ids: HashSet<OrderId>,
}

impl BuiltBlockInfo {
    pub fn new() -> Self {
        Self {
            orders_ids: HashSet::default(),
        }
    }

    pub fn add_order(&mut self, order: &Order) {
        self.orders_ids.insert(order.id());
    }

    pub fn contains_order(&self, order: &Order) -> bool {
        self.orders_ids.contains(&order.id())
    }
}

impl Default for BuiltBlockInfo {
    fn default() -> Self {
        Self::new()
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

    pub fn update_from_new_unfinished_block(&self, block: &dyn BlockBuildingHelper) {
        let mut block_info = BuiltBlockInfo::new();
        for execution_result in &block.built_block_trace().included_orders {
            block_info.add_order(&execution_result.order);
        }

        self.blocks_infos
            .lock()
            .insert(block.builder_name().to_string(), Arc::new(block_info));
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

impl Default for BuiltBlockCache {
    fn default() -> Self {
        Self::new()
    }
}
