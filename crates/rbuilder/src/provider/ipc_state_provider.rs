use std::{
    borrow::Cow,
    fmt::Debug,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use alloy_consensus::{constants::KECCAK_EMPTY, Header};
use alloy_eips::{BlockId, BlockNumHash, BlockNumberOrTag};
use alloy_json_rpc::RpcSend;
use alloy_primitives::{
    Address, BlockHash, BlockNumber, Bytes, StorageKey, StorageValue, B256, U256, U64,
};
use dashmap::DashMap;
use quick_cache::sync::Cache;
use reipc::rpc_provider::RpcProvider;
use reth_errors::{ProviderError, ProviderResult};
use reth_primitives::{Account, Bytecode};
use reth_provider::{
    errors::any::AnyError, AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider,
    StateProofProvider, StateProvider, StateProviderBox, StateRootProvider, StorageRootProvider,
};
use reth_trie::{
    updates::TrieUpdates, AccountProof, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
};
use revm::{
    database::{BundleAccount, BundleState},
    primitives::HashMap,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{trace, trace_span};

use crate::{
    building::ThreadBlockBuildingContext, live_builder::simulation::SimulatedOrderCommand,
    roothash::RootHashError,
};

use super::{RootHasher, StateProviderFactory};

/// After how many milliseconds should we give up on an IPC request (consider it failed)
/// 100ms was picked up after initial testing using Nethermind client as state provider
/// 99.9% requests return within 50ms; using 100ms gives us error rate of ~0.03%
/// Median response time is ~300 micro_sec.
const DEFAULT_IPC_REQUEST_TIMEOUT_MS: u64 = 100;
/// For how many blocks to cache state for
/// Most CL implementations keep state for last 128 blocks in memory,
/// We are mimicking this
const DEFAULT_STATE_CACHE_SIZE: usize = 128;

/// Remote state provider factory allows providing state via remote RPC calls over IPC
/// Specifically UnixDomainSockets
#[derive(Clone, Debug)]
pub struct IpcStateProviderFactory {
    ipc_provider: RpcProvider,

    code_cache: Arc<DashMap<B256, Bytecode>>,
    state_provider_by_hash: Arc<Cache<BlockHash, Arc<IpcStateProvider>>>,
}

impl IpcStateProviderFactory {
    /// Crates new IPC Provider Factory by establishing connection to IPC given path to the IPC
    /// req_timeout is the same across all requests to the IPC
    pub fn new(ipc_path: &Path, req_timeout: Duration) -> Self {
        let ipc_provider = RpcProvider::try_connect(ipc_path, req_timeout.into())
            // there is no need to gracefully handle (or propagate) this error, if we cannot connect
            // to IPC, then rbuilder cannot work
            .expect("can't connect to IPC");

        Self {
            ipc_provider,
            code_cache: Arc::new(DashMap::new()),
            state_provider_by_hash: Arc::new(Cache::new(DEFAULT_STATE_CACHE_SIZE)),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct IpcProviderConfig {
    pub(crate) request_timeout_ms: u64,
    pub(crate) ipc_path: PathBuf,
    pub(crate) mempool_server_url: String,
}

impl Default for IpcProviderConfig {
    fn default() -> Self {
        Self {
            request_timeout_ms: DEFAULT_IPC_REQUEST_TIMEOUT_MS,
            mempool_server_url: String::new(),
            ipc_path: PathBuf::new(),
        }
    }
}

impl StateProviderFactory for IpcStateProviderFactory {
    /// Gets state for the latest block
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        let state = IpcStateProvider::into_boxed(
            self.ipc_provider.clone(),
            BlockNumberOrTag::Latest.into(),
            self.code_cache.clone(),
        );

        Ok(state)
    }

    /// Gets state at the block number
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

    /// Gets state at the block hash
    fn history_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox> {
        if let Some(state) = self.state_provider_by_hash.get(&block) {
            return Ok(Box::new(state));
        }

        let state = IpcStateProvider::into_boxed(
            self.ipc_provider.clone(),
            block.into(),
            self.code_cache.clone(),
        );

        self.state_provider_by_hash.insert(block, *state.clone());
        Ok(state)
    }

    /// Gets block header given block hash
    fn header(&self, block_hash: &BlockHash) -> ProviderResult<Option<Header>> {
        let header = rpc_call::<
            (&BlockHash, bool),
            Option<<alloy_network::Ethereum as alloy_network::Network>::BlockResponse>,
        >(
            &self.ipc_provider,
            "eth_getBlockByHash",
            (block_hash, false),
        )?
        .map(|b| b.header.inner);

        Ok(header)
    }

    /// Gets block hash given block number
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        let block_hash = rpc_call::<(BlockNumberOrTag,), Option<B256>>(
            &self.ipc_provider,
            "rbuilder_getBlockHash",
            (BlockNumberOrTag::Number(number),),
        )?;

        Ok(block_hash)
    }

    /// Gets block number of latest known block
    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        self.last_block_number()
    }

    /// Gets block header given block hash
    fn header_by_number(&self, num: u64) -> ProviderResult<Option<Header>> {
        let block = rpc_call::<
            (u64, bool),
            Option<<alloy_network::Ethereum as alloy_network::Network>::BlockResponse>,
        >(&self.ipc_provider, "eth_getBlockByNumber", (num, false))?
        .map(|b| b.header.inner);

        Ok(block)
    }

    /// Gets block number of latest known block
    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        Ok(rpc_call::<(), U64>(&self.ipc_provider, "eth_blockNumber", ())?.to::<u64>())
    }

    /// Creates new root hasher - struct responsible for calculating root hash
    fn root_hasher(&self, parent_hash: BlockNumHash) -> ProviderResult<Box<dyn RootHasher>> {
        Ok(Box::new(StatRootHashCalculator {
            remote_provider: self.ipc_provider.clone(),
            parent_hash: parent_hash.hash,
        }))
    }
}

#[derive(Clone, Debug)]
pub struct IpcStateProvider {
    ipc_provider: RpcProvider,
    block_id: BlockId,

    // Per block cache
    block_hash_cache: DashMap<u64, BlockHash>,
    // Note: It's ok to cache Account (and Storage) even in case of None, this is because StateProvider gives the
    // state for some past block, so if account didn't exist the first time, it cannot magically
    // appear later on
    account_cache: DashMap<Address, Option<Account>>,
    storage_cache: DashMap<(Address, StorageKey), Option<StorageValue>>,

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
    // Note: this is known clippy issue: https://github.com/rust-lang/rust-clippy/issues/7472
    #[allow(clippy::redundant_allocation)]
    fn into_boxed(
        ipc_provider: RpcProvider,
        block_id: BlockId,
        code_cache: Arc<DashMap<B256, Bytecode>>,
    ) -> Box<Arc<Self>> {
        Box::new(Arc::new(Self::new(ipc_provider, block_id, code_cache)))
    }
}

impl BytecodeReader for IpcStateProvider {
    /// Get account code by its hash
    /// IMPORTANT: Assumes remote provider (node) has RPC call:"rbuilder_getCodeByHash"
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        let empty_hash = code_hash.is_zero() || *code_hash == KECCAK_EMPTY;
        if empty_hash {
            return Ok(None);
        }

        if let Some(bytecode) = self.code_cache.get(code_hash) {
            return Ok(Some(bytecode.clone()));
        }

        let bytecode = rpc_call::<(&B256,), Option<Bytes>>(
            &self.ipc_provider,
            "rbuilder_getCodeByHash",
            (code_hash,),
        )?
        .map(|b| {
            let bytecode = Bytecode::new_raw(b);
            self.code_cache.insert(*code_hash, bytecode.clone());
            bytecode
        });

        Ok(bytecode)
    }
}

impl StateProvider for IpcStateProvider {
    /// Get storage of given account
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        if let Some(storage) = self.storage_cache.get(&(account, storage_key)) {
            return Ok(*storage);
        }

        let key: U256 = storage_key.into();
        let storage = rpc_call(&self.ipc_provider, "eth_getStorageAt", (account, key))?;
        self.storage_cache.insert((account, storage_key), storage);

        Ok(storage)
    }
}

impl BlockHashReader for IpcStateProvider {
    /// Get the hash of the block with the given number. Returns `None` if no block with this number exists
    /// IMPORTANT: Assumes IPC provider (node) has RPC call:"rbuilder_getBlockHash"
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        if let Some(hash) = self.block_hash_cache.get(&number) {
            return Ok(Some(*hash));
        }

        let block_hash = rpc_call::<(BlockNumberOrTag,), Option<B256>>(
            &self.ipc_provider,
            "rbuilder_getBlockHash",
            (BlockNumberOrTag::Number(number),),
        )?;

        if let Some(bh) = block_hash {
            self.block_hash_cache.insert(number, bh);
        }

        Ok(block_hash)
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
        if let Some(account) = self.account_cache.get(address) {
            return Ok(*account);
        }

        let account = rpc_call::<(Address, BlockId), Option<AccountState>>(
            &self.ipc_provider,
            "rbuilder_getAccount",
            (*address, self.block_id),
        )?
        .map(|a| Account {
            nonce: a
                .nonce
                .try_into()
                .expect("Nonce received from RPC should fit u64"),
            bytecode_hash: a.code_hash.into(),
            balance: a.balance,
        });

        self.account_cache.insert(*address, account);

        Ok(account)
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

    fn witness(&self, _input: TrieInput, _target: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        unimplemented!()
    }
}

impl HashedPostStateProvider for IpcStateProvider {
    fn hashed_post_state(&self, _bundle_state: &BundleState) -> HashedPostState {
        unimplemented!()
    }
}

#[derive(Clone, Debug)]
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

    fn account_proofs(
        &self,
        _outcome: &BundleState,
        _addresses: &eth_sparse_mpt::utils::HashSet<Address>,
        _local_ctx: &mut ThreadBlockBuildingContext,
    ) -> Result<eth_sparse_mpt::utils::HashMap<Address, Vec<Bytes>>, RootHashError> {
        Err(RootHashError::Other(eyre::eyre!("method not implemented")))
    }

    /// Calculates the state root given changed accounts
    /// IMPORTANT: Assumes IPC provider (node) has RPC call:"rbuilder_calculateStateRoot"
    fn state_root(
        &self,
        outcome: &BundleState,
        _incremental_change: &[Address],
        _local_ctx: &mut ThreadBlockBuildingContext,
    ) -> Result<B256, RootHashError> {
        let account_diff: HashMap<Address, AccountDiff> = outcome
            .state
            .iter()
            .map(|(address, diff)| (*address, diff.clone().into()))
            .collect();

        let hash = rpc_call::<(BlockId, HashMap<Address, AccountDiff>), B256>(
            &self.remote_provider,
            "rbuilder_calculateStateRoot",
            (BlockId::Hash(self.parent_hash.into()), account_diff),
        )
        .map_err(|err| RootHashError::Other(err.into()))?;

        Ok(hash)
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
fn rpc_call<Param, Resp>(
    ipc_provider: &RpcProvider,
    rpc_method: impl Into<Cow<'static, str>> + tracing::Value,
    params: Param,
) -> ProviderResult<Resp>
where
    Param: RpcSend,
    Resp: DeserializeOwned + derive_more::with_trait::Debug,
{
    let span = trace_span!("rpc_call", rpc_method, id = rand::random::<u64>());
    let _guard = span.enter();
    trace!("send request");

    let resp = ipc_provider
        .call::<Param, Resp>(rpc_method, params)
        .map_err(ipc_to_provider_error);

    trace!("response received");
    resp
}

fn ipc_to_provider_error(e: reipc::errors::RpcError) -> ProviderError {
    ProviderError::Other(AnyError::new(e))
}
