use crate::{
    building::ThreadBlockBuildingContext, live_builder::simulation::SimulatedOrderCommand,
    roothash::RootHashError,
};
use alloy_consensus::Header;
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, BlockHash, BlockNumber, Bytes, B256};
use eth_sparse_mpt::utils::{HashMap, HashSet};
use reth_errors::ProviderResult;
use reth_provider::StateProviderBox;
use revm::database::BundleState;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

pub mod ipc_state_provider;
pub mod reth_prov;
pub mod state_provider_factory_from_provider_factory;

/// Main trait to interact with the chain data.
/// Allows to create different backends for chain data access without implementing lots of interfaces as would happen with reth_provider::StateProviderFactory
/// since it only asks for what we really use.
pub trait StateProviderFactory: Send + Sync {
    fn latest(&self) -> ProviderResult<StateProviderBox>;

    fn history_by_block_number(&self, block: BlockNumber) -> ProviderResult<StateProviderBox>;

    fn history_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox>;

    fn header(&self, block_hash: &BlockHash) -> ProviderResult<Option<Header>>;

    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>>;

    fn best_block_number(&self) -> ProviderResult<BlockNumber>;

    fn header_by_number(&self, num: u64) -> ProviderResult<Option<Header>>;

    fn last_block_number(&self) -> ProviderResult<BlockNumber>;

    fn root_hasher(&self, parent_num_hash: BlockNumHash) -> ProviderResult<Box<dyn RootHasher>>;
}

/// trait that computes the roothash for a new block assuming a predefine parent block (given in StateProviderFactory::root_hasher)
/// Ideally, it caches information in each roothash is computes (state_root) so the next one is faster.
/// Before using all run_prefetcher to allow the RootHasher start a prefetcher task that will pre cache root state trie nodes
/// based on what it sees on the simulations.
pub trait RootHasher: std::fmt::Debug + Send + Sync {
    /// Must be called once before using.
    /// This is too specific and prone to error (you may forget to call it), maybe it's a better idea to pass this to StateProviderFactory::root_hasher and let each RootHasher decide what to do?
    fn run_prefetcher(
        &self,
        simulated_orders: broadcast::Receiver<SimulatedOrderCommand>,
        cancel: CancellationToken,
    );

    /// State root for changes outcome on top of parent block.
    /// Incermental change is a list of accounts that are changed for the block since the last call to state_root
    fn state_root(
        &self,
        outcome: &BundleState,
        incremental_change: &[Address],
        local_ctx: &mut ThreadBlockBuildingContext,
    ) -> Result<B256, RootHashError>;

    /// Generate the account proof for the target address.
    /// NOTE: Proof targets are required to be loaded in the bundle state of [`ExecutionOutcome`].
    /// If the accounts are missing from the bundle state, the method will return "KeyNotFound" error.
    fn account_proofs(
        &self,
        outcome: &BundleState,
        addresses: &HashSet<Address>,
        local_ctx: &mut ThreadBlockBuildingContext,
    ) -> Result<HashMap<Address, Vec<Bytes>>, RootHashError>;
}
