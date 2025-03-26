use alloy_primitives::{BlockHash, B256};
use reth_provider::{ProviderResult, StateProviderBox, StateProviderFactory, StateRootProvider};
use reth_trie::updates::TrieUpdates;
use reth_trie::HashedPostState;

/// Main trait to interact with the chain data.
/// Allows to create different backends for chain data access without implementing lots of interfaces as would happen with reth_provider::StateProviderFactory
/// since it only asks for what we really use.
pub trait BuilderStateProviderFactory: Send + Sync {
    /// Returns _any_ [StateProvider] with matching block hash.
    ///
    /// This will return a [StateProvider] for either a historical or pending block.
    fn state_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox>;
}

impl<T: StateProviderFactory> BuilderStateProviderFactory for T {
    fn state_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox> {
        StateProviderFactory::state_by_block_hash(self, block)
    }
}

pub trait BuilderStateRootProvider: Send + Sync {
    /// Returns the state root of the `HashedPostState` on top of the current state with trie
    /// updates to be committed to the database.
    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)>;
}

impl<T: StateRootProvider> BuilderStateRootProvider for T {
    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        StateRootProvider::state_root_with_updates(self, hashed_state)
    }
}
