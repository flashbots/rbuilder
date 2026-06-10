use std::sync::{mpsc, Arc};

use crate::{
    building::ThreadBlockBuildingContext, live_builder::simulation::SimulatedOrderCommand,
    roothash::RootHashError,
};
use alloy_consensus::Header;
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, BlockHash, BlockNumber, Bytes, StorageKey, StorageValue, B256};
use eth_sparse_mpt::utils::{HashMap, HashSet};
use reth_errors::ProviderResult;
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, StateProofProvider,
    StateProvider, StateProviderBox, StateRootProvider, StorageRootProvider,
};
use reth_trie::{
    updates::TrieUpdates, AccountProof, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
};
use revm::database::BundleState;

pub mod ipc_state_provider;
pub mod reth_prov;
pub mod state_provider_factory_from_provider_factory;

/// A state provider shareable across builder threads.
pub type StateProviderArc = Arc<dyn StateProvider + Send + Sync>;

/// A [`StateProvider`] wrapper asserting that the wrapped provider is thread-safe.
///
/// reth 1.10 removed the `Sync` supertrait from [`StateProvider`]/`DbTx`
/// (paradigmxyz/reth#20516) so DB transactions can later grow single-threaded cursor
/// caches. As of reth v1.11.3 no such caches exist: MDBX transactions are still
/// internally synchronized (`TransactionPtr` keeps its lock and its `unsafe impl Sync`),
/// and the in-memory overlay / static-file / IPC providers hold only thread-safe data,
/// so sharing one provider across builder threads is as sound as it was before the
/// bound change.
///
/// SAFETY invariant: every [`StateProviderBox`] wrapped here must come from one of the
/// factories in this module (reth DB, reth node, IPC). Re-audit on every reth bump: if
/// DB transactions gain non-thread-safe internals this assertion becomes unsound.
pub struct SyncStateProvider(StateProviderBox);

// SAFETY: see type-level docs.
unsafe impl Sync for SyncStateProvider {}

impl SyncStateProvider {
    /// Wraps a provider produced by one of the factories in this module (see the
    /// type-level SAFETY invariant).
    pub fn new(provider: StateProviderBox) -> Self {
        Self(provider)
    }

    /// Like [`Self::new`] but returns a thread-shareable [`StateProviderArc`].
    pub fn new_arc(provider: StateProviderBox) -> StateProviderArc {
        Arc::new(Self::new(provider))
    }
}

impl BlockHashReader for SyncStateProvider {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.0.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.0.canonical_hashes_range(start, end)
    }
}

impl AccountReader for SyncStateProvider {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        self.0.basic_account(address)
    }
}

impl BytecodeReader for SyncStateProvider {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        self.0.bytecode_by_hash(code_hash)
    }
}

impl StateRootProvider for SyncStateProvider {
    fn state_root(&self, hashed_state: HashedPostState) -> ProviderResult<B256> {
        self.0.state_root(hashed_state)
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        self.0.state_root_from_nodes(input)
    }

    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.0.state_root_with_updates(hashed_state)
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.0.state_root_from_nodes_with_updates(input)
    }
}

impl StorageRootProvider for SyncStateProvider {
    fn storage_root(
        &self,
        address: Address,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        self.0.storage_root(address, hashed_storage)
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        self.0.storage_proof(address, slot, hashed_storage)
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        self.0.storage_multiproof(address, slots, hashed_storage)
    }
}

impl StateProofProvider for SyncStateProvider {
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        self.0.proof(input, address, slots)
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        self.0.multiproof(input, targets)
    }

    fn witness(&self, input: TrieInput, target: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        self.0.witness(input, target)
    }
}

impl HashedPostStateProvider for SyncStateProvider {
    fn hashed_post_state(&self, bundle_state: &BundleState) -> HashedPostState {
        self.0.hashed_post_state(bundle_state)
    }
}

impl StateProvider for SyncStateProvider {
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        self.0.storage(account, storage_key)
    }

    fn storage_by_hashed_key(
        &self,
        address: Address,
        hashed_storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        self.0.storage_by_hashed_key(address, hashed_storage_key)
    }
}

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
    fn run_prefetcher(&self, simulated_orders: mpsc::Receiver<SimulatedOrderCommand>);

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
