use reth_errors::ProviderResult;
use reth_primitives::BlockNumber;
use reth_provider::StateProviderBox;

mod http_provider;

pub trait StateProviderFactory: Send + Sync {
    fn history_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<StateProviderBox>;

    fn latest(&self) -> ProviderResult<StateProviderBox>;
}
