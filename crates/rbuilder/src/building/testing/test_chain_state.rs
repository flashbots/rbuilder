use crate::{
    building::BlockBuildingContext,
    live_builder::block_list_provider::BlockList,
    provider::RootHasher,
    roothash::RootHashContext,
    utils::{RootHasherImpl, Signer},
};
use alloy_consensus::{Block, Header, TxEip1559};
use alloy_primitives::{
    keccak256, utils::parse_ether, Address, BlockHash, Bytes, TxKind as TransactionKind, B256, B64,
    U256,
};
use alloy_rpc_types_beacon::events::{PayloadAttributesData, PayloadAttributesEvent};
use eth_sparse_mpt::ETHSpareMPTVersion::V2;
use lazy_static::lazy_static;
use reth::{
    primitives::{Account, BlockBody, Bytecode},
    providers::ProviderFactory,
    rpc::types::{engine::PayloadAttributes, Withdrawal},
};
use reth_chainspec::{ChainSpec, EthereumHardfork, MAINNET};
use reth_db::{cursor::DbCursorRW, tables, transaction::DbTxMut};
use reth_errors::ProviderResult;
use reth_primitives::{Recovered, TransactionSigned};
use reth_primitives_traits::Block as _;
use reth_provider::test_utils::{create_test_provider_factory, MockNodeTypesWithDB};
use revm::primitives::hardfork::SpecId;
use std::sync::Arc;

#[derive(Debug, Clone, Copy)]
pub enum NamedAddr {
    Builder,
    MevTest,
    BlockedAddress,
    FeeRecipient,
    User(usize),
    // Dummy address with no money and no code to receive empty txs
    Dummy,
}

#[derive(Debug)]
pub struct BlockArgs {
    pub number: u64,
    pub timestamp: u64,
}

impl Default for BlockArgs {
    fn default() -> Self {
        Self {
            number: 0,
            timestamp: EthereumHardfork::Cancun
                .mainnet_activation_timestamp()
                .unwrap(),
        }
    }
}

impl BlockArgs {
    pub fn number(self, number: u64) -> Self {
        Self { number, ..self }
    }

    pub fn timestamp(self, timestamp: u64) -> Self {
        Self { timestamp, ..self }
    }
}

/// Provides a fully working fake blockchain state with several pre-created accounts and contracts for testing
#[derive(Debug)]
pub struct TestChainState {
    builder: Signer,             //NamedAddr::Builder
    fee_recipient: Signer,       //NamedAddr::FeeRecipient
    test_accounts: Vec<Signer>,  //NamedAddr::User(i)
    mev_test_address: Address,   //NamedAddr::MevTest
    dummy_test_address: Address, //NamedAddr::Dummy
    blocklisted_address: Signer, //NamedAddr::BlockedAddress
    chain_spec: Arc<ChainSpec>,
    provider_factory: ProviderFactory<MockNodeTypesWithDB>,
    block_building_context: BlockBuildingContext,
}
pub struct ContractData {
    address: Address,
    code: Bytes,
    code_hash: B256,
}

impl ContractData {
    pub fn new(code: &Bytes, address: Address) -> Self {
        let code_hash = keccak256(code);
        ContractData {
            address,
            code: code.clone(),
            code_hash,
        }
    }
}

impl TestChainState {
    pub fn new(block_args: BlockArgs) -> eyre::Result<Self> {
        Self::new_with_balances_and_contracts(block_args, Default::default(), Default::default())
    }

    /// balances_to_increase this addresses start with that initial balances.
    /// extra_contracts are deploy along with the default mev_test.
    pub fn new_with_balances_and_contracts(
        block_args: BlockArgs,
        balances_to_increase: Vec<(Address, u128)>,
        extra_contracts: Vec<ContractData>,
    ) -> eyre::Result<Self> {
        let blocklisted_address = Signer::random();
        let builder = Signer::random();
        let fee_recipient = Signer::random();
        let chain_spec = MAINNET.clone();
        let test_accounts = vec![
            Signer::random(),
            Signer::random(),
            Signer::random(),
            Signer::random(),
            Signer::random(),
        ];
        let mev_test_address = Address::random();
        let dummy_test_address = Address::random();
        let test_contracts = TestContracts::load();

        let mut contracts = extra_contracts;
        contracts.push(test_contracts.mev_test(mev_test_address));

        let genesis_header = chain_spec.sealed_genesis_header();
        let provider_factory = create_test_provider_factory();
        {
            let provider = provider_factory.provider_rw()?;
            provider.insert_historical_block(
                Block::new(genesis_header.header().clone(), BlockBody::default())
                    .try_into_recovered()
                    .unwrap(),
            )?;

            {
                let mut cursor = provider
                    .tx_ref()
                    .cursor_write::<tables::PlainAccountState>()
                    .unwrap();
                let user_addresses = {
                    let mut res = Vec::new();
                    res.push(builder.address);
                    res.push(fee_recipient.address);
                    res.push(blocklisted_address.address);
                    res.extend(test_accounts.iter().map(|s| s.address));
                    res
                };
                for address in user_addresses {
                    cursor.upsert(
                        address,
                        &Account {
                            nonce: 0,
                            balance: parse_ether("1.0")?,
                            bytecode_hash: None,
                        },
                    )?;
                }
                // Failed to map user_addresses and chain it with balances_to_increase :(
                for (address, balance) in balances_to_increase {
                    cursor.upsert(
                        address,
                        &Account {
                            nonce: 0,
                            balance: U256::from(balance),
                            bytecode_hash: None,
                        },
                    )?;
                }

                for contract in &contracts {
                    cursor.upsert(
                        contract.address,
                        &Account {
                            nonce: 0,
                            balance: U256::ZERO,
                            bytecode_hash: Some(contract.code_hash),
                        },
                    )?;
                }
            }
            {
                let mut cursor = provider
                    .tx_ref()
                    .cursor_write::<tables::Bytecodes>()
                    .unwrap();
                for contract in &contracts {
                    cursor.upsert(
                        contract.code_hash,
                        &Bytecode::new_raw(contract.code.clone()),
                    )?;
                }
            }
            provider.commit()?;
        }

        let root_hasher = Arc::from(RootHasherImpl::new(
            genesis_header.num_hash(),
            None,
            RootHashContext::new(true, false, None, V2),
            provider_factory.clone(),
            provider_factory.clone(),
        ));

        let ctx = TestBlockContextBuilder::new(
            block_args,
            builder,
            fee_recipient.address,
            chain_spec.clone(),
            blocklisted_address.address,
            genesis_header.hash(),
            root_hasher,
        )
        .build();

        Ok(Self {
            builder,
            fee_recipient,
            chain_spec,
            blocklisted_address,
            test_accounts,
            mev_test_address,
            dummy_test_address,
            provider_factory,
            block_building_context: ctx,
        })
    }

    // returns signed transaction
    pub fn sign_tx(&self, args: TxArgs) -> eyre::Result<Recovered<TransactionSigned>> {
        let tx = TxEip1559 {
            chain_id: self.chain_spec.chain.id(),
            nonce: args.nonce,
            gas_limit: args.gas_limit,
            max_fee_per_gas: args.max_fee_per_gas,
            max_priority_fee_per_gas: args.max_priority_fee,
            to: match args.to {
                Some(named_addr) => TransactionKind::Call(self.named_address(named_addr)?),
                None => TransactionKind::Create,
            },
            value: U256::from(args.value),
            access_list: Default::default(),
            input: args.input.into(),
        };
        Ok(self.named_signer(args.account_idx)?.sign_tx(tx.into())?)
    }

    pub fn named_address(&self, named_addr: NamedAddr) -> eyre::Result<Address> {
        Ok(match named_addr {
            NamedAddr::Builder => self.builder.address,
            NamedAddr::MevTest => self.mev_test_address,
            NamedAddr::Dummy => self.dummy_test_address,
            NamedAddr::BlockedAddress => self.blocklisted_address.address,
            NamedAddr::FeeRecipient => self.fee_recipient.address,
            NamedAddr::User(idx) => {
                self.test_accounts
                    .get(idx)
                    .ok_or_else(|| eyre::eyre!("invalid user index"))?
                    .address
            }
        })
    }

    fn named_signer(&self, named_addr: NamedAddr) -> eyre::Result<&Signer> {
        Ok(match named_addr {
            NamedAddr::Builder => &self.builder,
            NamedAddr::MevTest => &self.builder,
            NamedAddr::Dummy => &self.builder, //Fake
            NamedAddr::BlockedAddress => &self.blocklisted_address,
            NamedAddr::FeeRecipient => &self.fee_recipient,
            NamedAddr::User(idx) => self
                .test_accounts
                .get(idx)
                .ok_or_else(|| eyre::eyre!("invalid user index"))?,
        })
    }
    pub fn block_building_context(&self) -> &BlockBuildingContext {
        &self.block_building_context
    }

    pub fn provider_factory(&self) -> &ProviderFactory<MockNodeTypesWithDB> {
        &self.provider_factory
    }

    pub fn upsert_contract(&self, address: Address, bytecode: Bytecode) -> ProviderResult<()> {
        let code_hash = bytecode.hash_slow();
        let provider = self.provider_factory.provider_rw()?;
        provider
            .tx_ref()
            .cursor_write::<tables::PlainAccountState>()?
            .upsert(
                address,
                &Account {
                    nonce: 1,
                    balance: U256::ZERO,
                    bytecode_hash: Some(code_hash),
                },
            )?;
        provider
            .tx_ref()
            .cursor_write::<tables::Bytecodes>()?
            .upsert(code_hash, &bytecode)?;
        provider.commit()?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct TestBlockContextBuilder {
    parent_gas_limit: u64,
    parent_timestamp: u64,
    block_number: u64,
    parent_base_fee_per_gas: u64,
    builder_signer: Signer,
    payload_attributes_version: String,
    slot_timestamp: u64,
    suggested_fee_recipient: Address,
    withdrawals: Option<Vec<Withdrawal>>,
    parent_gas_used: u64,
    parent_hash: BlockHash,
    chain_spec: Arc<ChainSpec>,
    blocklist: BlockList,
    prefer_gas_limit: Option<u64>,
    root_hasher: Arc<dyn RootHasher>,
    mev_block_price: U256,
}

impl TestBlockContextBuilder {
    fn new(
        block_args: BlockArgs,
        builder_signer: Signer,
        fee_recipient: Address,
        chain_spec: Arc<ChainSpec>,
        blocklisted: Address,
        parent_hash: BlockHash,
        root_hasher: Arc<dyn RootHasher>,
    ) -> Self {
        TestBlockContextBuilder {
            parent_gas_limit: 30_000_000,
            parent_timestamp: block_args.timestamp.checked_sub(12).unwrap_or_default(),
            block_number: block_args.number,
            parent_base_fee_per_gas: 1,
            builder_signer,
            payload_attributes_version: "capella".to_string(),
            slot_timestamp: block_args.timestamp,
            suggested_fee_recipient: fee_recipient,
            withdrawals: None,
            parent_gas_used: 15_000_000,
            parent_hash,
            chain_spec,
            blocklist: vec![blocklisted].into_iter().collect(),
            prefer_gas_limit: None,
            root_hasher,
            mev_block_price: U256::ZERO,
        }
    }

    fn build(self) -> BlockBuildingContext {
        BlockBuildingContext::from_attributes(
            PayloadAttributesEvent {
                version: self.payload_attributes_version,
                data: PayloadAttributesData {
                    proposal_slot: 1,
                    parent_block_root: Default::default(),
                    parent_block_number: 1,
                    parent_block_hash: self.parent_hash,
                    proposer_index: 0,
                    payload_attributes: PayloadAttributes {
                        timestamp: self.slot_timestamp,
                        prev_randao: Default::default(),
                        suggested_fee_recipient: self.suggested_fee_recipient,
                        withdrawals: self.withdrawals,
                        parent_beacon_block_root: None,
                    },
                },
            },
            &Header {
                parent_hash: self.parent_hash,
                ommers_hash: Default::default(),
                beneficiary: Default::default(),
                state_root: Default::default(),
                transactions_root: Default::default(),
                receipts_root: Default::default(),
                withdrawals_root: None,
                logs_bloom: Default::default(),
                difficulty: Default::default(),
                number: self.block_number.checked_sub(1).unwrap_or_default(),
                gas_limit: self.parent_gas_limit,
                gas_used: self.parent_gas_used,
                timestamp: self.parent_timestamp,
                mix_hash: Default::default(),
                nonce: B64::ZERO,
                base_fee_per_gas: Some(self.parent_base_fee_per_gas),
                blob_gas_used: None,
                excess_blob_gas: None,
                parent_beacon_block_root: None,
                extra_data: Default::default(),
                requests_hash: Default::default(),
            },
            self.builder_signer,
            self.chain_spec,
            self.blocklist,
            self.prefer_gas_limit,
            vec![],
            Some(SpecId::SHANGHAI),
            self.root_hasher,
            0,
            true,
            true,
            self.mev_block_price,
            Default::default(),
        )
        .unwrap()
    }
}

#[derive(Debug, Clone)]
pub struct TxArgs {
    account_idx: NamedAddr,
    to: Option<NamedAddr>,
    nonce: u64,
    value: u64,
    max_fee_per_gas: u128,
    max_priority_fee: u128,
    gas_limit: u64,
    input: Vec<u8>,
}

impl TxArgs {
    pub fn new(from: NamedAddr, nonce: u64) -> Self {
        Self {
            account_idx: from,
            to: None,
            nonce,
            value: 0,
            max_fee_per_gas: 1,
            max_priority_fee: 0,
            gas_limit: 1_000_000,
            input: Vec::new(),
        }
    }

    /// This transaction will revert if current slot value is not old slot value for the given slot
    pub fn new_increment_value(from: NamedAddr, nonce: u64, slot: u64, current_value: u64) -> Self {
        Self::new(from, nonce).to(NamedAddr::MevTest).input(
            [
                (*INCREMENT_VALUE_SELECTOR).into(),
                U256::from(slot).to_be_bytes_vec(),
                U256::from(current_value).to_be_bytes_vec(),
            ]
            .concat(),
        )
    }

    pub fn new_revert(from: NamedAddr, nonce: u64) -> Self {
        Self::new(from, nonce)
            .to(NamedAddr::MevTest)
            .input((*REVERT_SELECTOR).into())
    }

    /// This transaction send value to coinbase via a contract
    pub fn new_send_to_coinbase(from: NamedAddr, nonce: u64, value: u64) -> Self {
        Self::new(from, nonce)
            .to(NamedAddr::MevTest)
            .input((*SEND_TO_COINBASE_SELECTOR).into())
            .value(value)
    }

    /// This transaction send value to value_to via a contract
    pub fn new_send_to(from: NamedAddr, nonce: u64, value: u64, value_to: Address) -> Self {
        Self::new(from, nonce)
            .to(NamedAddr::MevTest)
            .input(
                [
                    (*SENT_TO_SELECTOR).into(),
                    B256::left_padding_from(value_to.as_slice()).to_vec(),
                ]
                .concat(),
            )
            .value(value)
    }

    /// This transaction for test purpose only, it reads balance of addr and the contract
    pub fn new_test_read_balance(from: NamedAddr, nonce: u64, addr: Address, value: u64) -> Self {
        Self::new(from, nonce)
            .to(NamedAddr::MevTest)
            .input(
                [
                    (*TEST_READ_BALANCE).into(),
                    B256::left_padding_from(addr.as_slice()).to_vec(),
                ]
                .concat(),
            )
            .value(value)
    }

    /// This transaction for test purpose only, it deploys a contract and let it selfdestruct within the tx.
    pub fn new_test_ephemeral_contract_destruct(
        from: NamedAddr,
        nonce: u64,
        refund_addr: Address,
    ) -> Self {
        Self::new(from, nonce).to(NamedAddr::MevTest).input(
            [
                (*TEST_EPHEMERAL_CONTRACT_DESTRUCT).into(),
                B256::left_padding_from(refund_addr.as_slice()).to_vec(),
            ]
            .concat(),
        )
    }

    pub fn to(self, to: NamedAddr) -> Self {
        Self {
            to: Some(to),
            ..self
        }
    }

    pub fn input(self, input: Vec<u8>) -> Self {
        Self { input, ..self }
    }

    /// If None current state nonce will be used
    pub fn nonce(self, nonce: u64) -> Self {
        Self { nonce, ..self }
    }

    pub fn value(self, value: u64) -> Self {
        Self { value, ..self }
    }

    pub fn max_fee_per_gas(self, max_fee_per_gas: u128) -> Self {
        Self {
            max_fee_per_gas,
            ..self
        }
    }

    pub fn max_priority_fee(self, max_priority_fee: u128) -> Self {
        Self {
            max_priority_fee,
            ..self
        }
    }

    pub fn gas_limit(self, gas_limit: u64) -> Self {
        Self { gas_limit, ..self }
    }
}

/// This contract was generated from mev-test-contract/src/MevTest.sol
static TEST_CONTRACTS: &str = include_str!("./contracts.json");

#[derive(Debug, serde::Deserialize)]
pub struct TestContracts {
    #[serde(rename = "MevTest")]
    mev_test: Bytes,

    #[serde(rename = "MevTestInitBytecode")]
    pub mev_test_init_bytecode: Bytes,
}

fn selector(func_signature: &str) -> [u8; 4] {
    keccak256(func_signature)[0..4].try_into().unwrap()
}

// Selectors for functions contained in the test contract (mev-test-contract/src/MevTest.sol).
lazy_static! {
    static ref INCREMENT_VALUE_SELECTOR: [u8; 4] = selector("incrementValue(uint256,uint256)");
    static ref SENT_TO_SELECTOR: [u8; 4] = selector("sendTo(address)");
    static ref SEND_TO_COINBASE_SELECTOR: [u8; 4] = selector("sendToCoinbase()");
    static ref REVERT_SELECTOR: [u8; 4] = selector("revert()");
    static ref TEST_READ_BALANCE: [u8; 4] = selector("testReadBalance(address)");
    static ref TEST_EPHEMERAL_CONTRACT_DESTRUCT: [u8; 4] =
        selector("testEphemeralContractDestruct(address)");
}

impl TestContracts {
    pub fn load() -> Self {
        serde_json::from_str(TEST_CONTRACTS).expect("failed to load test contracts")
    }

    fn mev_test(&self, address: Address) -> ContractData {
        ContractData::new(&self.mev_test, address)
    }
}
