use super::StateProviderFactory;
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_types::{BlockId, BlockNumberOrTag, BlockTransactionsKind};
use alloy_transport_http::Http;
use reqwest::Client;
use reth_errors::ProviderResult;
use reth_primitives::{
    Account, Address, Block, BlockHash, BlockNumber, Bytecode, Header, StorageKey, StorageValue,
    B256,
};
use reth_provider::{
    AccountReader, BlockHashReader, ExecutionOutcome, ProviderError, StateProofProvider,
    StateProvider, StateProviderBox, StateRootProvider,
};
use reth_trie::{updates::TrieUpdates, AccountProof, HashedPostState};
use tokio::runtime::Runtime;

#[derive(Clone)]
pub struct HttpProvider {
    provider: RootProvider<Http<Client>>,
}

impl HttpProvider {
    pub fn new_with_url(url: &str) -> Self {
        let provider = ProviderBuilder::new().on_http(url.parse().unwrap());

        Self { provider }
    }
}

impl StateProviderFactory for HttpProvider {
    fn history_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<StateProviderBox> {
        let res = Runtime::new()
            .unwrap()
            .block_on(async {
                self.provider
                    .get_block_by_number(BlockNumberOrTag::Number(block_number), false)
                    .await
            })
            .unwrap()
            .expect("a block");

        Ok(HttpProviderState::new(
            self.provider.clone(),
            res.header.hash.unwrap(),
        ))
    }

    fn latest(&self) -> ProviderResult<StateProviderBox> {
        let res = Runtime::new()
            .unwrap()
            .block_on(async {
                self.provider
                    .get_block(BlockId::latest(), BlockTransactionsKind::Hashes)
                    .await
            })
            .unwrap()
            .expect("a block");

        Ok(HttpProviderState::new(
            self.provider.clone(),
            res.header.hash.unwrap(),
        ))
    }

    fn history_by_block_hash(&self, block_hash: B256) -> ProviderResult<StateProviderBox> {
        let res = Runtime::new()
            .unwrap()
            .block_on(async {
                self.provider
                    .get_block_by_hash(block_hash, BlockTransactionsKind::Hashes)
                    .await
            })
            .unwrap()
            .expect("a block");

        Ok(HttpProviderState::new(
            self.provider.clone(),
            res.header.hash.unwrap(),
        ))
    }

    /// Get header by block hash
    fn header(&self, block_hash: &BlockHash) -> ProviderResult<Option<Header>> {
        let _res = Runtime::new()
            .unwrap()
            .block_on(async {
                self.provider
                    .get_block_by_hash(*block_hash, BlockTransactionsKind::Hashes)
                    .await
            })
            .unwrap()
            .expect("a block");

        unimplemented!("TODO")
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        unimplemented!("TODO")
    }

    fn block_by_number(&self, _num: u64) -> ProviderResult<Option<Block>> {
        unimplemented!("TODO")
    }

    fn state_root(
        &self,
        _parent_hash: B256,
        _output: &ExecutionOutcome,
    ) -> Result<B256, eyre::Error> {
        unimplemented!("TODO")
    }
}

pub struct HttpProviderState {
    provider: RootProvider<Http<Client>>,
    hash: B256,
}

impl HttpProviderState {
    pub fn new(provider: RootProvider<Http<Client>>, hash: B256) -> Box<Self> {
        Box::new(Self { provider, hash })
    }
}

impl StateProvider for HttpProviderState {
    /// Get storage of given account.
    fn storage(
        &self,
        address: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        let block_id = BlockId::hash(self.hash);

        let res = Runtime::new()
            .unwrap()
            .block_on(async {
                self.provider
                    .get_storage_at(address, storage_key.into())
                    .block_id(block_id)
                    .await
            })
            .unwrap();

        Ok(Some(res.into()))
    }

    /// Get account code by its hash
    fn bytecode_by_hash(&self, code_hash: B256) -> ProviderResult<Option<Bytecode>> {
        let address = Address::from_word(code_hash);
        let block_id = BlockId::hash(self.hash);

        let res = Runtime::new()
            .unwrap()
            .block_on(async { self.provider.get_code_at(address).block_id(block_id).await })
            .unwrap();

        Ok(Some(Bytecode::new_raw(res)))
    }
}

impl BlockHashReader for HttpProviderState {
    /// Get the hash of the block with the given number. Returns `None` if no block with this number
    /// exists.
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        let res = Runtime::new()
            .unwrap()
            .block_on(
                self.provider
                    .get_block_by_number(BlockNumberOrTag::Number(number), false),
            )
            .unwrap()
            .unwrap();

        Ok(res.header.hash)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        let mut res = vec![];

        for i in start..end {
            let block: alloy_rpc_types::Block = Runtime::new()
                .unwrap()
                .block_on(
                    self.provider
                        .get_block_by_number(BlockNumberOrTag::Number(i), false),
                )
                .unwrap()
                .unwrap();

            res.push(block.header.hash.unwrap());
        }

        Ok(res)
    }
}

impl AccountReader for HttpProviderState {
    fn basic_account(&self, address: Address) -> ProviderResult<Option<Account>> {
        let block_id = BlockId::hash(self.hash);

        let res: Result<Account, ProviderError> = Runtime::new().unwrap().block_on(async {
            let balance = self
                .provider
                .get_balance(address)
                .block_id(block_id)
                .await
                .unwrap();

            let nonce = self
                .provider
                .get_transaction_count(address)
                .block_id(block_id)
                .await
                .unwrap();

            Ok(Account {
                balance,
                nonce,
                bytecode_hash: Some(address.into_word()),
            })
        });

        let res = res.unwrap();
        Ok(Some(res))
    }
}

impl StateRootProvider for HttpProviderState {
    /// Returns the state root of the `HashedPostState` on top of the current state.
    fn hashed_state_root(&self, _hashed_state: &HashedPostState) -> ProviderResult<B256> {
        unimplemented!();
    }

    /// Returns the state root of the `HashedPostState` on top of the current state with trie
    /// updates to be committed to the database.
    fn hashed_state_root_with_updates(
        &self,
        _hashed_state: &HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        unimplemented!();
    }
}

impl StateProofProvider for HttpProviderState {
    /// Get account and storage proofs of target keys in the `HashedPostState`
    /// on top of the current state.
    fn hashed_proof(
        &self,
        _hashed_state: &HashedPostState,
        _address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        unimplemented!();
    }
}
