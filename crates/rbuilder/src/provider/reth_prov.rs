use crate::live_builder::simulation::SimulatedOrderCommand;
use crate::roothash::{calculate_state_root, run_trie_prefetcher, RootHashConfig, RootHashError};
use alloy_consensus::Header;
use alloy_primitives::{BlockHash, BlockNumber, B256};
use reth::providers::ExecutionOutcome;
use reth_errors::ProviderResult;
use reth_provider::StateProviderBox;
use reth_provider::{BlockReader, DatabaseProviderFactory, HeaderProvider};
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
        self.latest()
    }

    fn history_by_block_number(&self, block: BlockNumber) -> ProviderResult<StateProviderBox> {
        self.history_by_block_number(block)
    }

    fn history_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox> {
        self.history_by_block_hash(block)
    }

    fn header(&self, block_hash: &BlockHash) -> ProviderResult<Option<Header>> {
        self.header(block_hash)
    }

    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.block_hash(number)
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        self.best_block_number()
    }

    fn header_by_number(&self, num: u64) -> ProviderResult<Option<Header>> {
        self.header_by_number(num)
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        self.last_block_number()
    }

    fn run_trie_prefetcher(
        &self,
        parent_hash: B256,
        simulated_orders: broadcast::Receiver<SimulatedOrderCommand>,
        cancel: CancellationToken,
    ) {
        run_trie_prefetcher(
            parent_hash,
            Default::default(),
            self,
            simulated_orders,
            cancel,
        );
    }

    fn calculate_state_root(
        &self,
        parent_hash: B256,
        outcome: &ExecutionOutcome,
        config: RootHashConfig,
    ) -> Result<B256, RootHashError> {
        calculate_state_root(
            self.clone(),
            parent_hash,
            outcome,
            Default::default(),
            config,
        )
    }
}
