use crate::live_builder::simulation::SimulatedOrderCommand;
use crate::roothash::{RootHashConfig, RootHashError};
use alloy_consensus::Header;
use alloy_primitives::{BlockHash, BlockNumber, B256};
use reth::providers::ExecutionOutcome;
use reth_errors::ProviderResult;
use reth_provider::StateProviderBox;
use reth_provider::StateProviderFactory as A;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

pub mod reth_prov;

pub trait StateProviderFactory: Clone + 'static + Send + Sync {
    fn latest(&self) -> ProviderResult<StateProviderBox>;

    fn history_by_block_number(&self, block: BlockNumber) -> ProviderResult<StateProviderBox>;

    fn run_trie_prefetcher(
        &self,
        parent_hash: B256,
        simulated_orders: broadcast::Receiver<SimulatedOrderCommand>,
        cancel: CancellationToken,
    );

    fn history_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox>;

    fn header(&self, block_hash: &BlockHash) -> ProviderResult<Option<Header>>;

    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>>;

    fn best_block_number(&self) -> ProviderResult<BlockNumber>;

    fn header_by_number(&self, num: u64) -> ProviderResult<Option<Header>>;

    fn last_block_number(&self) -> ProviderResult<BlockNumber>;

    fn calculate_state_root(
        &self,
        parent_hash: B256,
        outcome: &ExecutionOutcome,
        config: RootHashConfig,
    ) -> Result<B256, RootHashError>;
}
