use crate::live_builder::simulation::SimulatedOrderCommand;
use crate::roothash::{RootHashConfig, RootHashError};
use alloy_consensus::Header;
use alloy_primitives::{BlockHash, BlockNumber, B256};
use reth::providers::ExecutionOutcome;
use reth_errors::ProviderResult;
use reth_provider::StateProviderBox;
use reth_provider::StateProviderFactory as A;
use reth_provider::{
    providers::{BlockchainProvider, BlockchainProvider2},
    BlockReader, DatabaseProviderFactory, HeaderProvider,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::StateProviderFactory;

impl<T> StateProviderFactory for T
where
    T: DatabaseProviderFactory<Provider: BlockReader>
        + reth_provider::StateProviderFactory
        + HeaderProvider
        + Clone
        + 'static,
{
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        unimplemented!()
    }

    fn history_by_block_number(&self, block: BlockNumber) -> ProviderResult<StateProviderBox> {
        unimplemented!()
    }

    fn run_trie_prefetcher(
        &self,
        parent_hash: B256,
        simulated_orders: broadcast::Receiver<SimulatedOrderCommand>,
        cancel: CancellationToken,
    ) {
        unimplemented!()
    }

    fn history_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox> {
        unimplemented!()
    }

    fn header(&self, block_hash: &BlockHash) -> ProviderResult<Option<Header>> {
        unimplemented!()
    }

    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        unimplemented!()
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        unimplemented!()
    }

    fn header_by_number(&self, num: u64) -> ProviderResult<Option<Header>> {
        unimplemented!()
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        unimplemented!()
    }

    fn calculate_state_root(
        &self,
        parent_hash: B256,
        outcome: &ExecutionOutcome,
        config: RootHashConfig,
    ) -> Result<B256, RootHashError> {
        unimplemented!()
    }
}
