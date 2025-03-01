use std::{
    fmt::Debug,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use alloy_consensus::{constants::KECCAK_EMPTY, Header};
use alloy_eips::{BlockId, BlockNumHash, BlockNumberOrTag};
use alloy_primitives::{BlockHash, BlockNumber, StorageKey, StorageValue, U64};
use dashmap::DashMap;
use reipc::rpc_provider::RpcProvider;
use reth_errors::{ProviderError, ProviderResult};
use reth_primitives::{Account, Bytecode};
use reth_provider::{
    AccountReader, BlockHashReader, HashedPostStateProvider, StateProofProvider, StateProvider,
    StateProviderBox, StateRootProvider, StorageRootProvider,
};
use reth_trie::{
    updates::TrieUpdates, AccountProof, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
};
use revm::db::{BundleAccount, BundleState};
use revm_primitives::{map::B256HashMap, Address, Bytes, HashMap, B256, U256};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{trace, trace_span};

use crate::live_builder::simulation::SimulatedOrderCommand;

use super::{RootHasher, StateProviderFactory};

/// After how many milliseconds should we give up on an IPC request (consider it failed)
/// 100ms was picked up after initial testing using Nethermind client as state provider
/// 99.9% requests return within 50ms; using 100ms gives us error rate of ~0.03%
/// Median response time is ~300 micro_sec.
const DEFAULT_IPC_REQUEST_TIMEOUT_MS: u64 = 100;

/// Remote state provider factory allows providing state via remote RPC calls over IPC
/// Specifically UnixDomainSockets
#[derive(Clone)]
pub struct IpcStateProviderFactory {
    ipc_provider: RpcProvider,

    code_cache: Arc<DashMap<B256, Bytecode>>,
    //TODO: invalidate cache for old blocks
    state_provider_by_hash: Arc<DashMap<BlockHash, Arc<IpcStateProvider>>>,
}

impl IpcStateProviderFactory {
    pub fn new(ipc_path: &Path, req_timeout: Duration) -> Self {
        let ipc_provider = RpcProvider::try_connect(ipc_path, req_timeout.into())
            // there is no need to gracefully handle (or propagate) this error, if we cannot connect
            // to IPC, then rbuilder cannot work
            .expect("Can't connect to IPC");

        Self {
            ipc_provider,
            code_cache: Arc::new(DashMap::new()),
            state_provider_by_hash: Arc::new(DashMap::new()),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct IpcProviderConfig {
    pub(crate) request_timeout: u64,
    pub(crate) ipc_path: PathBuf,
    pub(crate) txpool_server_url: String,
}

impl Default for IpcProviderConfig {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_IPC_REQUEST_TIMEOUT_MS,
            txpool_server_url: String::new(),
            ipc_path: PathBuf::new(),
        }
    }
}

impl StateProviderFactory for IpcStateProviderFactory {
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        let state = IpcStateProvider::into_boxed(
            self.ipc_provider.clone(),
            BlockNumberOrTag::Latest.into(),
            self.code_cache.clone(),
        );

        Ok(state)
    }

    // We are not caching state provider by block number to avoid any issues with reorgs
    // The calls to  history_by_block_number are rare and we shouldn't be loosing perf by not
    // leveraging caching here
    fn history_by_block_number(&self, block: BlockNumber) -> ProviderResult<StateProviderBox> {
        let state = IpcStateProvider::into_boxed(
            self.ipc_provider.clone(),
            block.into(),
            self.code_cache.clone(),
        );

        Ok(state)
    }

    fn history_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox> {
        if let Some(state) = self.state_provider_by_hash.get(&block) {
            return Ok(Box::new(state.clone()));
        }

        let state = IpcStateProvider::into_boxed(
            self.ipc_provider.clone(),
            block.into(),
            self.code_cache.clone(),
        );

        self.state_provider_by_hash.insert(block, *state.clone());
        Ok(state)
    }

    fn header(&self, block_hash: &BlockHash) -> ProviderResult<Option<Header>> {
        let id: u64 = rand::random();
        let span = trace_span!("header", id, block_hash = %block_hash.to_string());
        let _guard = span.enter();
        trace!("header: get");

        let header = self
            .ipc_provider
            .call::<_, Option<<alloy_network::Ethereum as alloy_network::Network>::BlockResponse>>(
                "eth_getBlockByHash",
                (block_hash, false),
            )
            .map_err(transport_to_provider_error)?
            .map(|b| b.header.inner);

        if header.is_none() {
            trace!("header: got none");
            return Ok(None);
        }

        trace!("header: got");

        Ok(header)
    }

    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        let id: u64 = rand::random();
        let span = trace_span!("block_hash provider factory", id, block_num = number);
        let _guard = span.enter();
        trace!("block_hash:get");

        let block_hash = self
            .ipc_provider
            .call::<_, B256>("rbuilder_getBlockHash", (BlockNumberOrTag::Number(number),))
            .map_err(transport_to_provider_error)?;

        trace!("block_hash: got");
        Ok(Some(block_hash))
    }

    //TODO: is this correct?
    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        self.last_block_number()
    }

    fn header_by_number(&self, num: u64) -> ProviderResult<Option<Header>> {
        let id: u64 = rand::random();
        let span = trace_span!("header_by_number", id, block_num = num);
        let _guard = span.enter();
        trace!("header_by_num:get");

        let block = self
            .ipc_provider
            .call::<_, Option<<alloy_network::Ethereum as alloy_network::Network>::BlockResponse>>(
                "eth_getBlockByNumber",
                (num, false),
            )
            .map_err(transport_to_provider_error)?;

        if block.is_none() {
            trace!("header_by_num: got none");
            return Ok(None);
        }

        trace!("header_by_num: got");

        Ok(block.map(|b| b.header.inner))
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        let id: u64 = rand::random();
        let span = trace_span!("last_block_num", id);
        let _guard = span.enter();
        trace!("last_block_num:get");

        let block_num = self
            .ipc_provider
            .call::<_, U64>("eth_blockNumber", ())
            .map_err(transport_to_provider_error)?
            .to::<u64>();

        trace!("last_block_num: got");
        Ok(block_num)
    }

    fn root_hasher(&self, parent_hash: BlockNumHash) -> ProviderResult<Box<dyn RootHasher>> {
        Ok(Box::new(StatRootHashCalculator {
            remote_provider: self.ipc_provider.clone(),
            parent_hash: parent_hash.hash,
        }))
    }
}

#[derive(Clone)]
pub struct IpcStateProvider {
    ipc_provider: RpcProvider,
    block_id: BlockId,

    //Per block cache
    block_hash_cache: DashMap<u64, BlockHash>,
    storage_cache: DashMap<(Address, StorageKey), StorageValue>,
    account_cache: DashMap<Address, Account>,

    // Global cache (cache not related to specific block)
    code_cache: Arc<DashMap<B256, Bytecode>>,
}

impl IpcStateProvider {
    /// Crates new instance of state provider
    fn new(
        ipc_provider: RpcProvider,
        block_id: BlockId,
        code_cache: Arc<DashMap<B256, Bytecode>>,
    ) -> Self {
        Self {
            ipc_provider,
            block_id,

            code_cache,

            block_hash_cache: DashMap::new(),
            storage_cache: DashMap::new(),
            account_cache: DashMap::new(),
        }
    }

    /// Crates new instance of state provider on the heap
    // Box::new(Arc::new(Self)) is required because StateProviderFactory returns Box<dyn StateProvider>
    fn into_boxed(
        ipc_provider: RpcProvider,
        block_id: BlockId,
        code_cache: Arc<DashMap<B256, Bytecode>>,
    ) -> Box<Arc<Self>> {
        Box::new(Arc::new(Self::new(ipc_provider, block_id, code_cache)))
    }
}

impl StateProvider for IpcStateProvider {
    /// Get storage of given account
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        let id: u64 = rand::random();
        let span = trace_span!("storage", id);
        let _guard = span.enter();
        trace!("storage:get");

        if let Some(storage) = self.storage_cache.get(&(account, storage_key)) {
            return Ok(Some(*storage));
        }

        let key: U256 = storage_key.into();
        let storage = self
            .ipc_provider
            .call::<_, StorageValue>("eth_getStorageAt", (account, key))
            .map_err(transport_to_provider_error)?;

        self.storage_cache.insert((account, storage_key), storage);

        trace!("got storage");
        Ok(Some(storage))
    }

    /// Get account code by its hash
    /// IMPORTANT: Assumes remote provider (node) has RPC call:"rbuilder_getCodeByHash"
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        let id: u64 = rand::random();
        let span = trace_span!("bytecode", id);
        let _guard = span.enter();
        trace!("bytecode:get");

        let empty_hash = code_hash.is_zero() || *code_hash == KECCAK_EMPTY;
        if empty_hash {
            return Ok(None);
        }

        if let Some(bytecode) = self.code_cache.get(code_hash) {
            return Ok(Some(bytecode.clone()));
        }

        let bytes = self
            .ipc_provider
            .call::<_, Bytes>("rbuilder_getCodeByHash", (code_hash,))
            .map_err(transport_to_provider_error)?;

        let bytecode = Bytecode::new_raw(bytes);

        self.code_cache.insert(*code_hash, bytecode.clone());
        trace!("bytecode: got");

        Ok(Some(bytecode))
    }
}

impl BlockHashReader for IpcStateProvider {
    /// Get the hash of the block with the given number. Returns `None` if no block with this number exists
    /// IMPORTANT: Assumes IPC provider (node) has RPC call:"rbuilder_getBlockHash"
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        let id: u64 = rand::random();
        let span = trace_span!("block_hash provider", id);
        let _guard = span.enter();
        trace!("block_hash: get");

        if let Some(hash) = self.block_hash_cache.get(&number) {
            return Ok(Some(*hash));
        }
        let block_hash = self
            .ipc_provider
            .call::<_, B256>("rbuilder_getBlockHash", (BlockNumberOrTag::Number(number),))
            .map_err(transport_to_provider_error)?;

        self.block_hash_cache.insert(number, block_hash);
        trace!("block_hash provider: got");
        Ok(Some(block_hash))
    }

    fn canonical_hashes_range(
        &self,
        _start: BlockNumber,
        _end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        unimplemented!()
    }
}

impl AccountReader for IpcStateProvider {
    /// Get basic account information.
    /// IMPORTANT: Assumes IPC provider (node) has RPC call:"rbuilder_getAccount"
    /// Returns `None` if the account doesn't exist.
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        let id: u64 = rand::random();
        let span = trace_span!("account", id, address = address.to_string());
        let _guard = span.enter();
        trace!("account: get");

        if let Some(account) = self.account_cache.get(address) {
            return Ok(Some(*account));
        }

        let account = self
            .ipc_provider
            .call::<_, AccountState>("rbuilder_getAccount", (*address, self.block_id))
            .map_err(transport_to_provider_error)?;

        let account = Account {
            nonce: account.nonce.try_into().unwrap(),
            bytecode_hash: account.code_hash.into(),
            balance: account.balance,
        };

        self.account_cache.insert(*address, account);

        trace!("account: got");
        Ok(Some(account))
    }
}

impl StateRootProvider for IpcStateProvider {
    fn state_root(&self, _hashed_state: HashedPostState) -> ProviderResult<B256> {
        unimplemented!()
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        unimplemented!()
    }

    fn state_root_with_updates(
        &self,
        _hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        unimplemented!()
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        unimplemented!()
    }
}

impl StorageRootProvider for IpcStateProvider {
    fn storage_root(
        &self,
        _address: Address,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        unimplemented!()
    }

    fn storage_proof(
        &self,
        _address: Address,
        _slot: B256,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        unimplemented!()
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        unimplemented!()
    }
}

impl StateProofProvider for IpcStateProvider {
    fn proof(
        &self,
        _input: TrieInput,
        _address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        unimplemented!()
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        unimplemented!()
    }

    fn witness(
        &self,
        _input: TrieInput,
        _target: HashedPostState,
    ) -> ProviderResult<B256HashMap<Bytes>> {
        unimplemented!()
    }
}

impl HashedPostStateProvider for IpcStateProvider {
    fn hashed_post_state(&self, _bundle_state: &BundleState) -> HashedPostState {
        unimplemented!()
    }
}

#[derive(Clone)]
pub struct StatRootHashCalculator {
    remote_provider: RpcProvider,
    parent_hash: B256,
}

impl RootHasher for StatRootHashCalculator {
    fn run_prefetcher(
        &self,
        _simulated_orders: broadcast::Receiver<SimulatedOrderCommand>,
        _cancel: CancellationToken,
    ) {
        unimplemented!()
    }

    /// Calculates the state root given changed accounts
    /// IMPORTANT: Assumes IPC provider (node) has RPC call:"rbuilder_calculateStateRoot"
    fn state_root(
        &self,
        outcome: &reth_provider::ExecutionOutcome,
    ) -> Result<B256, crate::roothash::RootHashError> {
        let id: u64 = rand::random();
        let span = trace_span!("state_root", id, block = outcome.first_block);
        let _guard = span.enter();
        trace!("state_root: get");

        let account_diff: HashMap<Address, AccountDiff> = outcome
            .bundle
            .state
            .iter()
            .map(|(address, diff)| (*address, diff.clone().into()))
            .collect();

        let hash = self
            .remote_provider
            .call::<_, B256>(
                "rbuilder_calculateStateRoot",
                (BlockId::Hash(self.parent_hash.into()), account_diff),
            )
            .map_err(|_| crate::roothash::RootHashError::Verification)?;

        trace!("state_root: got");

        Ok(hash)
    }
}

impl derive_more::Debug for StatRootHashCalculator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatRootHashCalculator").finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AccountDiff {
    pub nonce: Option<U256>,
    pub balance: Option<U256>,
    pub self_destructed: bool,
    pub changed_slots: HashMap<U256, U256>,
    pub code_hash: Option<B256>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AccountState {
    pub nonce: U256,
    pub balance: U256,
    pub code_hash: B256,
}

impl From<BundleAccount> for AccountDiff {
    fn from(value: BundleAccount) -> Self {
        let self_destructed = value.was_destroyed();

        let changed_slots = value
            .storage
            .iter()
            .map(|(k, v)| (*k, v.present_value))
            .collect();

        match value.info {
            Some(info) => Self {
                changed_slots,
                self_destructed,
                balance: Some(info.balance),
                nonce: Some(U256::from(info.nonce)),
                code_hash: Some(info.code_hash),
            },
            None => Self {
                changed_slots,
                self_destructed,
                balance: None,
                nonce: None,
                code_hash: None,
            },
        }
    }
}

//TODO: this is temp hack, fix it properly
fn transport_to_provider_error(e: reipc::errors::RpcError) -> ProviderError {
    ProviderError::Database(reth_db::DatabaseError::Other(format!("IPC ERROR: {e}")))
}
