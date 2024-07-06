use super::StateProviderFactory;
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_types::{BlockId, BlockNumberOrTag};
use alloy_transport_http::Http;
use reqwest::Client;
use reth_interfaces::provider::ProviderResult;
use reth_primitives::{
    trie::AccountProof, Account, Address, BlockNumber, Bytecode, StorageKey, StorageValue, B256,
};
use reth_provider::{
    AccountReader, BlockHashReader, StateProvider, StateProviderBox, StateRootProvider,
};
use reth_trie::updates::TrieUpdates;
use revm::db::BundleState;
use tokio::runtime::Runtime;

struct HttpProvider {
    provider: RootProvider<Http<Client>>,
}

impl HttpProvider {
    pub fn new_with_url(url: &str) -> Self {
        let provider = ProviderBuilder::new()
            .on_http(url.parse().unwrap())
            .unwrap();

        Self { provider }
    }

    fn get_block_number_by_tag(&self, tag: BlockNumberOrTag) -> ProviderResult<BlockNumber> {
        let rt = Runtime::new()
            .unwrap()
            .block_on(self.provider.get_block_by_number(tag, false))
            .unwrap()
            .unwrap();

        Ok(rt.header.number.unwrap())
    }
}

impl StateProviderFactory for HttpProvider {
    fn history_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<StateProviderBox> {
        self.provider
            .get_block_by_number(BlockNumberOrTag::Number(block_number), false);

        unimplemented!("TODO")
    }

    fn latest(&self) -> ProviderResult<StateProviderBox> {
        unimplemented!("TODO")
    }
}

pub struct HttpProviderState {
    provider: RootProvider<Http<Client>>,
    hash: B256,
}

impl HttpProviderState {
    pub fn new(provider: RootProvider<Http<Client>>, hash: B256) -> Self {
        Self { provider, hash }
    }
}

impl StateProvider for HttpProviderState {
    /// Get storage of given account.
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        let res = Runtime::new()
            .unwrap()
            .block_on(self.provider.get_storage_at(
                account,
                storage_key.into(),
                BlockId::hash(self.hash),
            ))
            .unwrap();

        Ok(Some(res))
    }

    /// Get account code by its hash
    fn bytecode_by_hash(&self, code_hash: B256) -> ProviderResult<Option<Bytecode>> {
        // find the specific address
        let address = Address::from_word(code_hash);

        let res = Runtime::new()
            .unwrap()
            .block_on(self.provider.get_code_at(address, BlockId::hash(self.hash)))
            .unwrap();

        Ok(Some(Bytecode::new_raw(res)))
    }

    /// Get account and storage proofs.
    fn proof(&self, address: Address, keys: &[B256]) -> ProviderResult<AccountProof> {
        let keys: Vec<B256> = keys.to_vec();

        let res = Runtime::new()
            .unwrap()
            .block_on(
                self.provider
                    .get_proof(address, keys, BlockId::hash(self.hash)),
            )
            .unwrap();

        unimplemented!("todo");
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

#[derive(Debug)]
enum AccountError {}

impl AccountReader for HttpProviderState {
    fn basic_account(&self, address: Address) -> ProviderResult<Option<Account>> {
        let res: Result<Account, AccountError> = Runtime::new().unwrap().block_on(async {
            let balance = self
                .provider
                .get_balance(address, BlockId::hash(self.hash))
                .await
                .unwrap();

            let nonce = self
                .provider
                .get_transaction_count(address, BlockId::hash(self.hash))
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
    fn state_root(&self, _bundle_state: &BundleState) -> ProviderResult<B256> {
        unimplemented!("todo");
    }

    /// Returns the state root of the BundleState on top of the current state with trie
    /// updates to be committed to the database.
    fn state_root_with_updates(
        &self,
        _bundle_state: &BundleState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        unimplemented!("todo");
    }
}
