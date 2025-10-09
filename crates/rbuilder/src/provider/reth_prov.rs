use crate::{roothash::RootHashContext, utils::RootHasherImpl};
use alloy_consensus::Header;
use alloy_eips::BlockNumHash;
use alloy_primitives::{BlockHash, BlockNumber, B256};
use reth_errors::ProviderResult;
use reth_provider::{BlockReader, DatabaseProviderFactory, HeaderProvider, StateProviderBox};
use tracing::error;

use super::{RootHasher, StateProviderFactory};

/// StateProviderFactory based on a reth traits.
#[derive(Clone)]
pub struct StateProviderFactoryFromRethProvider<P> {
    provider: P,
    root_hash_context: RootHashContext,
}

impl<P> StateProviderFactoryFromRethProvider<P> {
    pub fn new(provider: P, root_hash_context: RootHashContext) -> Self {
        Self {
            provider,
            root_hash_context,
        }
    }
}

impl<P> StateProviderFactory for StateProviderFactoryFromRethProvider<P>
where
    P: DatabaseProviderFactory<Provider: BlockReader>
        + reth_provider::StateProviderFactory
        + HeaderProvider<Header = Header>
        + Clone
        + 'static,
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
        let hasher = self.history_by_block_hash(parent_num_hash.hash)?;
        let parent_state_root = self
            .provider
            .header_by_hash_or_number(parent_num_hash.hash.into())?
            .map(|h| h.state_root);
        if parent_state_root.is_none() {
            error!("Parent hash is not found (for root_hasher)");
        }
        Ok(Box::new(RootHasherImpl::new(
            parent_num_hash,
            parent_state_root,
            self.root_hash_context.clone(),
            self.provider.clone(),
            hasher,
        )))
    }
}
