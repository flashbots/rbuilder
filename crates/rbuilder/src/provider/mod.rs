use reth_errors::ProviderResult;
use reth_primitives::{Block, BlockHash, BlockNumber, Header, B256};
use reth_provider::{ExecutionOutcome, StateProviderBox};

pub mod http_provider;

pub trait StateProviderFactory: Send + Sync {
    fn history_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<StateProviderBox>;

    /// Get header by block hash
    fn header(&self, block_hash: &BlockHash) -> ProviderResult<Option<Header>>;

    fn history_by_block_hash(&self, block_hash: B256) -> ProviderResult<StateProviderBox>;

    fn last_block_number(&self) -> ProviderResult<BlockNumber>;

    fn latest(&self) -> ProviderResult<StateProviderBox>;

    fn block_by_number(&self, num: u64) -> ProviderResult<Option<Block>>;

    fn state_root(&self, parent_hash: B256, output: &ExecutionOutcome)
        -> Result<B256, eyre::Error>; // TODO: Custom error
}
