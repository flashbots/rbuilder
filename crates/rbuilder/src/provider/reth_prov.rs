use crate::roothash::RootHashConfig;
use crate::utils::RootHasherImpl;
use alloy_consensus::Header;
use alloy_primitives::{BlockHash, BlockNumber, B256};
use reth_errors::ProviderResult;
use reth_provider::{BlockReader, DatabaseProviderFactory, HeaderProvider};
use reth_provider::{StateCommitmentProvider, StateProviderBox};

use super::{RootHasher, StateProviderFactory};

/// StateProviderFactory based on a reth traits.
#[derive(Clone)]
pub struct StateProviderFactoryFromRethProvider<P> {
    provider: P,
    config: RootHashConfig,
}

impl<P> StateProviderFactoryFromRethProvider<P> {
    pub fn new(provider: P, config: RootHashConfig) -> Self {
        Self { provider, config }
    }
}

impl<P> StateProviderFactory for StateProviderFactoryFromRethProvider<P>
where
    P: DatabaseProviderFactory<Provider: BlockReader>
        + reth_provider::StateProviderFactory
        + HeaderProvider<Header = Header>
        + StateCommitmentProvider
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

    fn root_hasher(&self, parent_hash: B256) -> ProviderResult<Box<dyn RootHasher>> {
        let hasher = self.history_by_block_hash(parent_hash)?;
        Ok(Box::new(RootHasherImpl::new(
            parent_hash,
            self.config.clone(),
            self.provider.clone(),
            hasher,
        )))
    }
}
