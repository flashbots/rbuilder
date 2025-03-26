use alloy_primitives::BlockHash;
use reth_provider::{ProviderResult, StateProviderBox, StateProviderFactory};

/// Main trait to interact with the chain data.
/// Allows to create different backends for chain data access without implementing lots of interfaces as would happen with reth_provider::StateProviderFactory
/// since it only asks for what we really use.
pub trait BuilderStateProviderFactory: Send + Sync {
    /// Returns _any_ [StateProvider] with matching block hash.
    ///
    /// This will return a [StateProvider] for either a historical or pending block.
    fn state_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox>;
}

impl <T: StateProviderFactory> BuilderStateProviderFactory for T {
    fn state_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox> {
        StateProviderFactory::state_by_block_hash(self, block)
    }
}
