use reth_errors::ProviderResult;
use reth_primitives::{BlockNumber, B256};
use reth_provider::StateProviderBox;

mod http_provider;

pub trait StateProviderFactory: Send + Sync {
    fn history_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<StateProviderBox>;

    fn history_by_block_hash(&self, block_hash: B256) -> ProviderResult<StateProviderBox>;

    fn last_block_number(&self) -> ProviderResult<BlockNumber>;

    fn latest(&self) -> ProviderResult<StateProviderBox>;
}
