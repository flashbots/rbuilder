use alloy_consensus::Header;
use alloy_eips::BlockNumHash;
use alloy_primitives::{BlockHash, BlockNumber, B256};
use reth_errors::ProviderResult;
use reth_node_api::{NodePrimitives, NodeTypes, NodeTypesWithDB};
use reth_provider::{
    providers::ProviderNodeTypes, BlockHashReader, BlockNumReader, HeaderProvider, ProviderFactory,
    StateProviderBox,
};
use tracing::error;

use crate::{
    building::builders::mock_block_building_helper::MockRootHasher, roothash::RootHashContext,
    utils::RootHasherImpl,
};

use super::{RootHasher, StateProviderFactory};

/// StateProviderFactory based on a ProviderFactory.
#[derive(Clone)]
pub struct StateProviderFactoryFromProviderFactory<N: NodeTypesWithDB> {
    provider: ProviderFactory<N>,
    root_hash_context: Option<RootHashContext>,
}

impl<N: NodeTypesWithDB> StateProviderFactoryFromProviderFactory<N> {
    /// root_hash_config None -> no roothash (MockRootHasher)
    pub fn new(provider: ProviderFactory<N>, root_hash_context: Option<RootHashContext>) -> Self {
        Self {
            provider,
            root_hash_context,
        }
    }
}

impl<N> StateProviderFactory for StateProviderFactoryFromProviderFactory<N>
where
    N: NodeTypesWithDB + ProviderNodeTypes + Clone,
    <N as NodeTypes>::Primitives: NodePrimitives<BlockHeader = Header>,
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

    fn root_hasher(&self, parent_num_hash: BlockNumHash) -> ProviderResult<Box<dyn RootHasher>> {
        Ok(if let Some(root_hash_context) = &self.root_hash_context {
            let parent_state_root = self
                .provider
                .header_by_hash_or_number(parent_num_hash.hash.into())?
                .map(|h| h.state_root);
            if parent_state_root.is_none() {
                error!("Parent hash is not found (for root_hasher)");
            }
            Box::new(RootHasherImpl::new(
                parent_num_hash,
                parent_state_root,
                root_hash_context.clone(),
                self.provider.clone(),
                self.provider.clone(),
            ))
        } else {
            Box::new(MockRootHasher {})
        })
    }
}
