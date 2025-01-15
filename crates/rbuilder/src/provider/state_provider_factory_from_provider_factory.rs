use alloy_consensus::Header;
use alloy_primitives::{BlockHash, BlockNumber};
use reth_errors::ProviderResult;
use reth_node_api::NodeTypesWithDB;
use reth_provider::{
    providers::ProviderNodeTypes, BlockHashReader, BlockNumReader, HeaderProvider, ProviderFactory,
    StateProviderBox,
};
use revm_primitives::B256;

use crate::{
    building::builders::mock_block_building_helper::MockRootHasher, roothash::RootHashConfig,
    utils::RootHasherImpl,
};

use super::{RootHasher, StateProviderFactory};

/// StateProviderFactory based on a ProviderFactory.
#[derive(Clone)]
pub struct StateProviderFactoryFromProviderFactory<N: NodeTypesWithDB> {
    provider: ProviderFactory<N>,
    root_hash_config: Option<RootHashConfig>,
}

impl<N: NodeTypesWithDB> StateProviderFactoryFromProviderFactory<N> {
    /// root_hash_config None -> no roothash (MockRootHasher)
    pub fn new(provider: ProviderFactory<N>, root_hash_config: Option<RootHashConfig>) -> Self {
        Self {
            provider,
            root_hash_config,
        }
    }
}

impl<N: NodeTypesWithDB + ProviderNodeTypes + Clone> StateProviderFactory
    for StateProviderFactoryFromProviderFactory<N>
{
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        self.provider.latest()
    }

    fn history_by_block_number(&self, block: BlockNumber) -> ProviderResult<StateProviderBox> {
        self.provider.history_by_block_number(block)
    }

    fn history_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox> {
        self.provider.history_by_block_hash(block)
    }

    fn header(&self, block_hash: &BlockHash) -> ProviderResult<Option<Header>> {
        self.provider.header(block_hash)
    }

    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.provider.block_hash(number)
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        self.provider.best_block_number()
    }

    fn header_by_number(&self, num: u64) -> ProviderResult<Option<Header>> {
        self.provider.header_by_number(num)
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        self.provider.last_block_number()
    }

    fn root_hasher(&self, parent_hash: B256) -> Box<dyn RootHasher> {
        if let Some(root_hash_config) = &self.root_hash_config {
            Box::new(RootHasherImpl::new(
                parent_hash,
                root_hash_config.clone(),
                self.provider.clone(),
            ))
        } else {
            Box::new(MockRootHasher {})
        }
    }
}
