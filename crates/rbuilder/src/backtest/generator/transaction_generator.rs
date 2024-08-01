use alloy_primitives::{address, Address, Bytes, FixedBytes, TxKind, B256, U256};
use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;
use reth::{
    primitives::{
        AccessList,
        public_key_to_address, sign_message, Transaction, TransactionKind, TransactionSigned,
        TransactionSignedEcRecovered, TxEip1559, TxEip4844, TxLegacy, MAINNET,
    },
};
use secp256k1::{SecretKey, SECP256K1};
use std::collections::HashMap;

use super::{
    conflict_generator::distribute_transactions,
    solidity_bindings::{
        precompile_blake2fCall, precompile_bn128_addCall, precompile_bn128_mulCall,
        precompile_bn256_pairingCall, precompile_ecrecoverCall, precompile_identityCall,
        precompile_modExpCall, precompile_ripemd160Call, precompile_sha256Call,
        readModifyWriteCall, read_coinbase_write_slotCall, writeToMultipleSlotsCall,
        writeToMultipleSlotsProprtionalCall, writeToSlotCall, writeToSlotProportionalCall,
    },
};
use uuid::Uuid;

const MAX_PAYMENT: u64 = 100_000_000_000_000_000; // 0.1 ETH
const MAX_UPPER_BOUND: u64 = 1000;
const DEFAULT_NUM_SIGNERS: usize = 10;
const DEFAULT_GAS_LIMIT: u64 = 100_000;
const DEFAULT_BASE_FEE: u128 = 1_000_000_000_000;
const DEFAULT_PRIO_FEE: u128 = 1_000_000_000;
const MAX_FUNCTION_NUMBER: u8 = 5;
const CONTRACT_ADDRESS: Address = address!("444111f638c62F3bAFBd5C578688e6282C9710ba");

use crate::primitives::{Bundle, MempoolTx, Order, TransactionSignedEcRecoveredWithBlobs};

use alloy_sol_types::SolCall;

#[derive(Debug, Clone, Copy)]
pub enum TransactionType {
    Legacy,
    Eip1559,
    Eip4844,
}

// implement a default for TransactionType
impl Default for TransactionType {
    fn default() -> Self {
        TransactionType::Legacy
    }
}

#[derive(Debug, Copy, Clone)]
pub enum Precompile {
    Ecrecover,
    Sha256,
    Ripemd160,
    Identity,
    Modexp,
    Bn128Add,
    Bn128Mul,
    Bn256Pairing,
    Blake2f,
}

#[derive(Debug, Copy, Clone)]
pub enum TransactionExecutionType {
    Precompile(Precompile),
    Size(usize),
    Conflict,
    None,
}

impl Default for TransactionExecutionType {
    fn default() -> Self {
        TransactionExecutionType::None
    }
}

impl From<Precompile> for TransactionExecutionType {
    fn from(precompile: Precompile) -> Self {
        TransactionExecutionType::Precompile(precompile)
    }
}

pub struct Size(pub usize);

impl From<Size> for TransactionExecutionType {
    fn from(size: Size) -> Self {
        TransactionExecutionType::Size(size.0)
    }
}

pub struct TransactionGenerator {
    rng: StdRng,
    /// The set of signer keys available for transaction generation.
    pub signer_keys: Vec<B256>,
    /// The base fee for transactions.
    pub base_fee: u128,
    /// The gas limit for transactions.
    pub gas_limit: u64,
    /// The type of execution for the transaction.
    pub transaction_type: TransactionType,
    pub transaction_execution_type: TransactionExecutionType,
    pub bundle_uuid_id: u128,
}

impl TransactionGenerator {
    pub fn new(random_seed: u64) -> Self {
        Self::with_num_signers(random_seed, DEFAULT_NUM_SIGNERS)
    }

    /// Generates random random signers
    pub fn with_num_signers(random_seed: u64, num_signers: usize) -> Self {
        let rng = StdRng::seed_from_u64(random_seed);
        Self {
            rng,
            signer_keys: (0..num_signers).map(|_| B256::random()).collect(),
            base_fee: DEFAULT_BASE_FEE,
            gas_limit: DEFAULT_GAS_LIMIT,
            transaction_type: TransactionType::default(),
            transaction_execution_type: TransactionExecutionType::default(),
            bundle_uuid_id: 0,
        }
    }

    pub fn get_signer_addresses(&self) -> Vec<Address> {
        self.signer_keys.iter().map(|key| get_address(*key)).collect()
    }

    /// Sets the default gas limit for all generated transactions
    pub fn set_gas_limit(&mut self, gas_limit: u64) -> &mut Self {
        self.gas_limit = gas_limit;
        self
    }

    /// Sets the base fee for the generated transactions
    pub fn set_base_fee(&mut self, base_fee: u64) -> &mut Self {
        self.base_fee = base_fee as u128;
        self
    }

    pub fn set_transaction_type(&mut self, transaction_type: TransactionType) -> &mut Self {
        self.transaction_type = transaction_type;
        self
    }

    pub fn set_transaction_execution_type<T: Into<TransactionExecutionType>>(
        &mut self,
        transaction_execution_type: T,
    ) -> &mut Self {
        self.transaction_execution_type = transaction_execution_type.into();
        self
    }

    pub fn generate_conflicting_transactions(
        &mut self,
        num_of_slots: usize,
        num_of_transaction: usize,
        gradiant: f64,
    ) -> Vec<Order> {
        let transactions_per_slot = distribute_transactions(num_of_slots, num_of_transaction, gradiant);
        self.generate_transactions_for_slots(transactions_per_slot)
    }

    fn generate_transactions_for_slots(&mut self, transactions_per_slot: Vec<usize>) -> Vec<Order> {
        let mut transactions_result = Vec::new();
        let mut nonces: HashMap<B256, u64> = HashMap::new();

        for (slot, num_transactions) in transactions_per_slot.iter().enumerate() {
            if *num_transactions == 0 {
                continue;
            }
            for _ in 0..*num_transactions {
                let transaction = self.generate_single_transaction(slot, &mut nonces);
                transactions_result.push(transaction);
            }
        }

        transactions_result
    }

    fn generate_single_transaction(&mut self, slot: usize, nonces: &mut HashMap<B256, u64>) -> Order {
        let signer = self.rng_signer();
        let nonce = nonces.entry(signer).or_insert(0);

        let function_num = get_function_number(&mut self.rng);
        let calldata = process_function_number(slot as u64, function_num, &mut self.rng);

        println!("Bundle UUID: {:?}", self.bundle_uuid_id);

        let tx = self
            .build_transaction()
            .signer(signer)
            .nonce(*nonce)
            .input(calldata)
            .to(CONTRACT_ADDRESS)
            .into_bundle(self.bundle_uuid_id);

        *nonce += 1;
        self.bundle_uuid_id += 1;

        tx
    }
    // pub fn transaction_with_execution_type<T: Into<TransactionExecutionType>>(
    //     &mut self,
    //     tx_type: T,
    // ) -> TransactionBuilder {
    //     let tx_type = tx_type.into();
    //     let mut tx = self.build_transaction();
    //     tx.to = TxKind::Call(Address::from([1; 20]));

    //     match tx_type {
    //         TransactionExecutionType::Precompile(precompile) => {
    //             tx.input(self.handle_generate_precompile(precompile))
    //         }
    //         TransactionExecutionType::Size(size_in_kilobytes) => {
    //             tx.input(self.handle_generate_size_calldata(size_in_kilobytes))
    //         }
    //         TransactionExecutionType::Conflict => {
    //             // Set the transaction to have a conflicting nonce
    //             tx
    //         }
    //         TransactionExecutionType::None => tx,
    //     }
    // }

    fn get_call_data(&mut self) -> Bytes {
        match &self.transaction_execution_type {
            TransactionExecutionType::Precompile(precompile) => {
                handle_generate_precompile(*precompile)
            }
            TransactionExecutionType::Size(size) => {
                handle_generate_size_calldata(*size, &mut self.rng)
            }
            TransactionExecutionType::Conflict => todo!(),
            TransactionExecutionType::None => Bytes::from(vec![0x0]),
        }
    }

    /// Creates a new transaction with a random signer
    /// Creates a new transaction with a random signer
    pub fn build_transaction(&mut self) -> TransactionBuilder {
        TransactionBuilder::default()
            .signer(self.rng_signer())
            .max_fee_per_gas(self.base_fee)
            .max_priority_fee_per_gas(self.base_fee)
            .gas_limit(self.gas_limit)
            .set_transaction_type(self.transaction_type)
            .input(self.get_call_data())
    }

    /// Returns a random signer from the set
    fn rng_signer(&mut self) -> B256 {
        // Use random_seed to generate a random signer
        let idx = self.rng.gen_range(0..self.signer_keys.len() as u64);

        self.signer_keys[idx as usize]
    }
}

/// A Builder type to configure and create a transaction.
#[derive(Debug)]
pub struct TransactionBuilder {
    /// The type of transaction to be created (Legacy, EIP-1559, EIP-4844)
    pub transaction_type: TransactionType,
    /// The type of execution for the transaction.
    pub execution_type: TransactionExecutionType,
    /// The signer used to sign the transaction.
    pub signer: B256,
    /// The chain ID on which the transaction will be executed.
    pub chain_id: u64,
    /// The nonce value for the transaction to prevent replay attacks.
    pub nonce: u64,
    /// The maximum amount of gas units that the transaction can consume.
    pub gas_limit: u64,
    /// The maximum fee per gas unit that the sender is willing to pay.
    pub max_fee_per_gas: u128,
    /// The maximum priority fee per gas unit that the sender is willing to pay for faster
    /// processing.
    pub max_priority_fee_per_gas: u128,
    /// The recipient or contract address of the transaction.
    pub to: TxKind,
    /// The value to be transferred in the transaction.
    pub value: U256,
    /// The list of addresses and storage keys that the transaction can access.
    pub access_list: AccessList,
    /// The input data for the transaction, typically containing function parameters for contract
    /// calls.
    pub input: Bytes,
    pub bundle_uuid_id: u128,
}

pub fn handle_generate_precompile(precompile: Precompile) -> Bytes {
    match precompile {
        Precompile::Ecrecover => {
            // TO-DO: FIX
            let call = precompile_ecrecoverCall {
                hash: FixedBytes([0u8; 32]),
                v: 0,
                r: FixedBytes([0u8; 32]),
                s: FixedBytes([0u8; 32]),
            };
            call.abi_encode().into()
        }
        Precompile::Sha256 => {
            let call = precompile_sha256Call {
                input: Bytes::from(vec![0x00]),
            };
            call.abi_encode().into()
        }
        Precompile::Ripemd160 => {
            let call = precompile_ripemd160Call {
                input: Bytes::from(vec![0x00]),
            };
            call.abi_encode().into()
        }
        Precompile::Identity => {
            let call = precompile_identityCall {
                input: Bytes::from(vec![0x00]),
            };
            call.abi_encode().into()
        }
        Precompile::Modexp => {
            let call = precompile_modExpCall {
                b: U256::from(0),
                e: U256::from(0),
                m: U256::from(0),
            };
            call.abi_encode().into()
        }
        Precompile::Bn128Add => {
            let call = precompile_bn128_addCall {
                ax: FixedBytes([0u8; 32]),
                ay: FixedBytes([0u8; 32]),
                bx: FixedBytes([0u8; 32]),
                by: FixedBytes([0u8; 32]),
            };
            call.abi_encode().into()
        }
        Precompile::Bn128Mul => {
            let call = precompile_bn128_mulCall {
                x: FixedBytes([0u8; 32]),
                y: FixedBytes([0u8; 32]),
                scalar: FixedBytes([0u8; 32]),
            };
            call.abi_encode().into()
        }
        Precompile::Bn256Pairing => {
            let call = precompile_bn256_pairingCall {
                input: Bytes::from(vec![0x00]),
            };
            call.abi_encode().into()
        }
        Precompile::Blake2f => {
            let call = precompile_blake2fCall {
                rounds: 0,
                h: [FixedBytes([0u8; 32]), FixedBytes([0u8; 32])],
                m: [
                    FixedBytes([0u8; 32]),
                    FixedBytes([0u8; 32]),
                    FixedBytes([0u8; 32]),
                    FixedBytes([0u8; 32]),
                ],
                t: [FixedBytes([0u8; 8]), FixedBytes([0u8; 8])],
                f: false,
            };
            call.abi_encode().into()
        }
    }
}

pub fn get_storage_contention_calldata_inputs(rng: &mut StdRng) -> (U256, U256, U256, U256) {
    let (lower_bound, upper_bound) = get_bounds(rng);
    let new_value = get_new_value(rng, lower_bound, upper_bound);
    let payment = get_payment(rng);

    (
        U256::from(lower_bound),
        U256::from(upper_bound),
        U256::from(new_value),
        U256::from(payment),
    )
}

pub fn get_function_number(rng: &mut StdRng) -> u8 {
    let function_num = rng.gen_range(0..=MAX_FUNCTION_NUMBER);
    function_num as u8
}

pub fn get_bounds(rng: &mut StdRng) -> (u64, u64) {
    let lower_bound = 0;
    let upper_bound = rng.gen_range(lower_bound + 1..MAX_UPPER_BOUND);
    (lower_bound, upper_bound)
}

pub fn get_new_value(rng: &mut StdRng, lower_bound: u64, upper_bound: u64) -> u64 {
    rng.gen_range(lower_bound..3)
}

pub fn get_payment(rng: &mut StdRng) -> u64 {
    rng.gen_range(0..MAX_PAYMENT)
    // 0
}

pub fn process_function_number(slot: u64, function_num: u8, rng: &mut StdRng) -> Bytes {
    println!("processing function_num: {:?}", function_num);

    let calldata = match function_num{
        0=> {
            println!("handle_write_to_slot");
            handle_write_to_slot(slot, rng) // 0xb60ab2a8
        }
        1=> {
            println!("handle_write_to_slot_proportional");
            handle_write_to_slot_proportional(slot, rng) //0xb82a721b
        }
        2=> {
            println!("handle_write_to_multiple_slots");
            handle_write_to_multiple_slots(slot, rng) // 0x3b45d703
        }
        3=> {
            println!("handle_write_to_multiple_slots_proportional");
            handle_write_to_multiple_slots_proportional(slot, rng) // 0x26ccecfd
        }
        4=> {
            println!("handle_read_coinbase_write_slot");
            handle_read_coinbase_write_slot(slot, rng) // 0x11d5d1f4
        }
        5=> {
            println!("handle_read_modify_write");
            handle_read_modify_write(slot, rng) // 0xd252124f
        }
        _=> unreachable!(),
    };

    println!("calldata: {:?}", calldata);
    // let calldata = match function_num {
    //     0 => handle_write_to_slot(slot, rng),
    //     1 => handle_write_to_slot_proportional(slot, rng), // 0xb82a721b
    //     2 => handle_write_to_multiple_slots(slot, rng),
    //     3 => handle_write_to_multiple_slots_proportional(slot, rng),
    //     4 => handle_read_coinbase_write_slot(slot, rng), // 0x11d5d1f4
    //     5 => handle_read_modify_write(slot, rng),        // 0xd252124f
    //     _ => unreachable!(),
    // };
    calldata
}

fn handle_read_modify_write(slot: u64, rng: &mut StdRng) -> Bytes {
    let (_, _, new_value, _) = get_storage_contention_calldata_inputs(rng);
    println!("new_value: {:?}", new_value);
    let call = readModifyWriteCall {
        slot: U256::from(slot),
        modifierValue: new_value,
    };
    call.abi_encode().into()
}

fn handle_read_coinbase_write_slot(slot: u64, rng: &mut StdRng) -> Bytes {
    let (
        lower_bound_current_expected_value,
        upper_bound_current_expected_value,
        new_value,
        payment,
        expected_coinbase_value,
        top_of_block_value,
    ) = get_read_coinbase_write_slot_calldata_inputs(rng);
    println!("lower_bound_current_expected_value: {:?}, upper_bound_current_expected_value: {:?}, new_value: {:?}, payment: {:?}, expected_coinbase_value: {:?}, top_of_block_value: {:?}",
        lower_bound_current_expected_value, upper_bound_current_expected_value, new_value, payment, expected_coinbase_value, top_of_block_value
    );
    let call = read_coinbase_write_slotCall {
        slot: U256::from(slot),
        topOfBlockValue: top_of_block_value,
        expectedCoinbaseValue: expected_coinbase_value,
        lowerBoundCurrentExpectedValue: lower_bound_current_expected_value,
        upperBoundCurrentExpectedValue: upper_bound_current_expected_value,
        newValue: new_value,
        payment: payment,
    };
    call.abi_encode().into()
}

fn get_read_coinbase_write_slot_calldata_inputs(
    rng: &mut StdRng,
) -> (U256, U256, U256, U256, U256, U256) {
    let (lower_bound, upper_bound) = get_bounds(rng);
    let new_value = get_new_value(rng, lower_bound, upper_bound);
    let payment = get_payment(rng);
    let expected_coinbase_value = U256::from(0);
    let top_of_block_value = U256::from(rng.gen_range(payment.into()..MAX_PAYMENT));
    (
        U256::from(lower_bound),
        U256::from(upper_bound),
        U256::from(new_value),
        U256::from(payment),
        U256::from(expected_coinbase_value),
        U256::from(top_of_block_value),
    )
}

fn handle_write_to_multiple_slots_proportional(slot: u64, rng: &mut StdRng) -> Bytes {
    let (
        lower_bound_current_expected_value,
        upper_bound_current_expected_value,
        new_value,
        payment,
    ) = get_storage_contention_calldata_inputs(rng);
    let mut call = writeToMultipleSlotsProprtionalCall {
        slotsArray: vec![U256::from(slot)],
        lowerBounds: vec![lower_bound_current_expected_value],
        upperBounds: vec![upper_bound_current_expected_value],
        newValues: vec![new_value],
        payments: vec![payment],
    };
    println!(
        "lower_bound_current_expected_value: {:?}, upper_bound_current_expected_value: {:?}, new_value: {:?}, payment: {:?}",
        lower_bound_current_expected_value, upper_bound_current_expected_value, new_value, payment
    );
    for _ in 0..5 {
        let new_slot = rng.gen_range(0..slot + 1);
        if call.slotsArray.contains(&U256::from(new_slot)) {
            println!("skipping slot {:?} because it is already in slotsArray", new_slot);
            continue;
        }
        let (
            lower_bound_current_expected_value,
            upper_bound_current_expected_value,
            new_value,
            payment,
        ) = get_storage_contention_calldata_inputs(rng);
        println!("slot: {:?}", new_slot);
        println!(
            "lower_bound_current_expected_value: {:?}, upper_bound_current_expected_value: {:?}, new_value: {:?}, payment: {:?}",
            lower_bound_current_expected_value, upper_bound_current_expected_value, new_value, payment
        );
        call.slotsArray.push(U256::from(new_slot));
        call.lowerBounds.push(lower_bound_current_expected_value);
        call.upperBounds.push(upper_bound_current_expected_value);
        call.newValues.push(new_value);
        call.payments.push(payment);
    }
    call.abi_encode().into()
}

fn handle_write_to_multiple_slots(slot: u64, rng: &mut StdRng) -> Bytes {
    let (
        lower_bound_current_expected_value,
        upper_bound_current_expected_value,
        new_value,
        payment,
    ) = get_storage_contention_calldata_inputs(rng);
    let mut call = writeToMultipleSlotsCall {
        slotsArray: vec![U256::from(slot)],
        lowerBounds: vec![U256::from(lower_bound_current_expected_value)],
        upperBounds: vec![U256::from(upper_bound_current_expected_value)],
        newValues: vec![U256::from(new_value)],
        payments: vec![U256::from(payment)],
    };
    println!(
        "lower_bound_current_expected_value: {:?}, upper_bound_current_expected_value: {:?}, new_value: {:?}, payment: {:?}",
        lower_bound_current_expected_value, upper_bound_current_expected_value, new_value, payment);
    for _ in 0..5 {
        let new_slot = rng.gen_range(0..slot + 1);
        if call.slotsArray.contains(&U256::from(new_slot)) {
            continue;
        }
        println!("slot: {:?}", new_slot);
        println!(
            "lower_bound_current_expected_value: {:?}, upper_bound_current_expected_value: {:?}, new_value: {:?}, payment: {:?}",
            lower_bound_current_expected_value, upper_bound_current_expected_value, new_value, payment
        );
        let (
            lower_bound_current_expected_value,
            upper_bound_current_expected_value,
            new_value,
            payment,
        ) = get_storage_contention_calldata_inputs(rng);
        call.slotsArray.push(U256::from(new_slot));
        call.lowerBounds
            .push(U256::from(lower_bound_current_expected_value));
        call.upperBounds
            .push(U256::from(upper_bound_current_expected_value));
        call.newValues.push(U256::from(new_value));
        call.payments.push(U256::from(payment));
    }
    call.abi_encode().into()
}

pub fn handle_write_to_slot(slot: u64, rng: &mut StdRng) -> Bytes {
    let (
        lower_bound_current_expected_value,
        upper_bound_current_expected_value,
        new_value,
        payment,
    ) = get_storage_contention_calldata_inputs(rng);
    println!(
        "lower_bound_current_expected_value: {:?}, upper_bound_current_expected_value: {:?}, new_value: {:?}, payment: {:?}",
        lower_bound_current_expected_value, upper_bound_current_expected_value, new_value, payment
    );
    let call = writeToSlotCall {
        slot: U256::from(slot),
        lowerBoundCurrentExpectedValue: lower_bound_current_expected_value,
        upperBoundCurrentExpectedValue: upper_bound_current_expected_value,
        newValue: new_value,
        payment: payment,
    };
    call.abi_encode().into()
}

fn handle_write_to_slot_proportional(slot: u64, rng: &mut StdRng) -> Bytes {
    let (
        lower_bound_current_expected_value,
        upper_bound_current_expected_value,
        new_value,
        payment,
    ) = get_storage_contention_calldata_inputs(rng);
    println!(
        "lower_bound_current_expected_value: {:?}, upper_bound_current_expected_value: {:?}, new_value: {:?}, payment: {:?}",
        lower_bound_current_expected_value, upper_bound_current_expected_value, new_value, payment
    );
    let call = writeToSlotProportionalCall {
        slot: U256::from(slot),
        lowerBoundCurrentExpectedValue: lower_bound_current_expected_value,
        upperBoundCurrentExpectedValue: upper_bound_current_expected_value,
        newValue: new_value,
        payment: payment,
    };
    call.abi_encode().into()
}

pub fn handle_generate_size_calldata(size_in_kilobytes: usize, rng: &mut StdRng) -> Bytes {
    let num_bytes = size_in_kilobytes * 1024;

    let random_bytes: Vec<u8> = (0..num_bytes).map(|_| rng.gen()).collect();
    random_bytes.into()
}

impl TransactionBuilder {
    /// Sets the signer for the transaction builder.
    pub const fn signer(mut self, signer: B256) -> Self {
        self.signer = signer;
        self
    }

    /// Sets the gas limit for the transaction builder.
    pub const fn gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = gas_limit;
        self
    }

    /// Sets the nonce for the transaction builder.
    pub const fn nonce(mut self, nonce: u64) -> Self {
        self.nonce = nonce;
        self
    }

    /// Increments the nonce value of the transaction builder by 1.
    pub fn inc_nonce(mut self) -> Self {
        self.nonce += 1;
        self
    }

    /// Decrements the nonce value of the transaction builder by 1, avoiding underflow.
    pub fn decr_nonce(mut self) -> Self {
        self.nonce = self.nonce.saturating_sub(1);
        self
    }

    /// Sets the maximum fee per gas for the transaction builder.
    pub const fn max_fee_per_gas(mut self, max_fee_per_gas: u128) -> Self {
        self.max_fee_per_gas = max_fee_per_gas;
        self
    }

    /// Sets the maximum priority fee per gas for the transaction builder.
    pub const fn max_priority_fee_per_gas(mut self, max_priority_fee_per_gas: u128) -> Self {
        self.max_priority_fee_per_gas = max_priority_fee_per_gas;
        self
    }

    /// Sets the recipient or contract address for the transaction builder.
    pub const fn to(mut self, to: Address) -> Self {
        self.to = TxKind::Call(to);
        self
    }

    /// Sets the value to be transferred in the transaction.
    pub fn value(mut self, value: u128) -> Self {
        self.value = U256::from(value);
        self
    }

    /// Sets the access list for the transaction builder.
    pub fn access_list(mut self, access_list: AccessList) -> Self {
        self.access_list = access_list;
        self
    }

    /// Sets the transaction input data.
    pub fn input(mut self, input: impl Into<Bytes>) -> Self {
        self.input = input.into();
        self
    }

    /// Sets the chain ID for the transaction.
    pub const fn chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = chain_id;
        self
    }

    /// Sets the chain ID for the transaction, mutable reference version.
    pub fn set_chain_id(&mut self, chain_id: u64) -> &mut Self {
        self.chain_id = chain_id;
        self
    }

    /// Sets the nonce for the transaction, mutable reference version.
    pub fn set_nonce(&mut self, nonce: u64) -> &mut Self {
        self.nonce = nonce;
        self
    }

    /// Sets the gas limit for the transaction, mutable reference version.
    pub fn set_gas_limit(&mut self, gas_limit: u64) -> &mut Self {
        self.gas_limit = gas_limit;
        self
    }

    /// Sets the maximum fee per gas for the transaction, mutable reference version.
    pub fn set_max_fee_per_gas(&mut self, max_fee_per_gas: u128) -> &mut Self {
        self.max_fee_per_gas = max_fee_per_gas;
        self
    }

    /// Sets the maximum priority fee per gas for the transaction, mutable reference version.
    pub fn set_max_priority_fee_per_gas(&mut self, max_priority_fee_per_gas: u128) -> &mut Self {
        self.max_priority_fee_per_gas = max_priority_fee_per_gas;
        self
    }

    /// Sets the recipient or contract address for the transaction, mutable reference version.
    pub fn set_to(&mut self, to: Address) -> &mut Self {
        self.to = to.into();
        self
    }

    /// Sets the value to be transferred in the transaction, mutable reference version.
    pub fn set_value(&mut self, value: u128) -> &mut Self {
        self.value = U256::from(value);
        self
    }

    /// Sets the access list for the transaction, mutable reference version.
    pub fn set_access_list(&mut self, access_list: AccessList) -> &mut Self {
        self.access_list = access_list;
        self
    }

    /// Sets the signer for the transaction, mutable reference version.
    pub fn set_signer(&mut self, signer: B256) -> &mut Self {
        self.signer = signer;
        self
    }

    /// Sets the transaction input data, mutable reference version.
    pub fn set_input(&mut self, input: impl Into<Bytes>) -> &mut Self {
        self.input = input.into();
        self
    }

    pub fn set_transaction_type(mut self, transaction_type: TransactionType) -> Self {
        self.transaction_type = transaction_type;
        self
    }

    pub fn into_mempool_tx(self) -> Order {
        let tx = match self.transaction_type {
            TransactionType::Legacy => self.into_legacy(),
            TransactionType::Eip1559 => self.into_eip1559(),
            TransactionType::Eip4844 => self.into_eip4844(),
        };
        Order::Tx(MempoolTx::new(tx))
    }

    pub fn into_bundle(self, uuid_num: u128) -> Order {
        let uuid_bytes = uuid_num.to_be_bytes();
        let tx = match self.transaction_type {
            TransactionType::Legacy => self.into_legacy(),
            TransactionType::Eip1559 => self.into_eip1559(),
            TransactionType::Eip4844 => self.into_eip4844(),
        };

        let bundle = Bundle {
            txs: vec![tx.clone()],
            hash: tx.hash(),
            reverting_tx_hashes: vec![],
            block: 20141086,
            uuid: Uuid::from_bytes(uuid_bytes),
            min_timestamp: None,
            max_timestamp: None,
            replacement_data: None,
            signer: None,
            metadata: Default::default(),
        };

        Order::Bundle(bundle)
    }

    /// Signs the provided transaction using the specified signer and returns a signed transaction.
    fn signed(transaction: Transaction, signer: B256) -> TransactionSigned {
        let signature = sign_message(signer, transaction.signature_hash()).unwrap();
        TransactionSigned::from_transaction_and_signature(transaction, signature)
    }

    pub fn into_legacy(self) -> TransactionSignedEcRecoveredWithBlobs {
        let signed_ec_recovered = signed_into_signed_ec_recovered(
            get_address(self.signer),
            self.get_legacy_signed_transaction(),
        );
        TransactionSignedEcRecoveredWithBlobs::new_no_blobs(signed_ec_recovered).unwrap()
    }

    pub fn get_legacy_signed_transaction(self) -> TransactionSigned {
        Self::signed(
            TxLegacy {
                chain_id: Some(self.chain_id),
                nonce: self.nonce,
                gas_limit: self.gas_limit,
                gas_price: self.max_fee_per_gas,
                to: match self.to {
                    TxKind::Call(to) => TransactionKind::Call(to),
                    TxKind::Create => TransactionKind::Call(Address::default()),
                },
                value: self.value,
                input: self.input,
            }
            .into(),
            self.signer,
        )
    }

    pub fn get_eip1559_signed_transaction(self) -> TransactionSigned {
        Self::signed(
            TxEip1559 {
                chain_id: self.chain_id,
                nonce: self.nonce,
                gas_limit: self.gas_limit,
                max_fee_per_gas: self.max_fee_per_gas,
                max_priority_fee_per_gas: self.max_priority_fee_per_gas,
                to: match self.to {
                    TxKind::Call(to) => TransactionKind::Call(to),
                    TxKind::Create => TransactionKind::Call(Address::default()),
                },
                value: self.value,
                access_list: self.access_list,
                input: self.input,
            }
            .into(),
            self.signer,
        )
    }

    pub fn get_eip4844_signed_transaction(self) -> TransactionSigned {
        Self::signed(
            TxEip4844 {
                chain_id: self.chain_id,
                nonce: self.nonce,
                gas_limit: self.gas_limit,
                max_fee_per_gas: self.max_fee_per_gas,
                max_priority_fee_per_gas: self.max_priority_fee_per_gas,
                to: match self.to {
                    TxKind::Call(to) => TransactionKind::Call(to),
                    TxKind::Create => TransactionKind::Call(Address::default()),
                },
                value: self.value,
                access_list: self.access_list,
                input: self.input,
                blob_versioned_hashes: Default::default(),
                max_fee_per_blob_gas: Default::default(),
            }
            .into(),
            self.signer.clone(),
        )
    }
    /// Converts the transaction builder into a transaction format using EIP-1559.
    pub fn into_eip1559(self) -> TransactionSignedEcRecoveredWithBlobs {
        let signed_ec_recovered = signed_into_signed_ec_recovered(
            get_address(self.signer),
            self.get_eip1559_signed_transaction(),
        );
        TransactionSignedEcRecoveredWithBlobs::new_no_blobs(signed_ec_recovered).unwrap()
    }

    /// Converts the transaction builder into a transaction format using EIP-4844.
    // TO-DO: ADD BLOBS and maybe fix other non-blobs
    // TO-DO: FIX  THIS IS BROKEN
    pub fn into_eip4844(self) -> TransactionSignedEcRecoveredWithBlobs {
        let signed_ec_recovered = signed_into_signed_ec_recovered(
            get_address(self.signer),
            self.get_eip4844_signed_transaction(),
        );
        TransactionSignedEcRecoveredWithBlobs::new_no_blobs(signed_ec_recovered).unwrap()
    }
}

fn signed_into_signed_ec_recovered(
    address: Address,
    signed_transaction: TransactionSigned,
) -> TransactionSignedEcRecovered {
    TransactionSignedEcRecovered::from_signed_transaction(signed_transaction, address)
}

fn get_address(signer: B256) -> Address {
    let secret = SecretKey::from_slice(signer.as_ref()).unwrap();
    let pubkey = secret.public_key(SECP256K1);
    public_key_to_address(pubkey)
}

impl Default for TransactionBuilder {
    fn default() -> Self {
        Self {
            execution_type: TransactionExecutionType::Conflict,
            transaction_type: TransactionType::default(),
            signer: B256::random(),
            chain_id: MAINNET.chain.id(),
            nonce: 0,
            gas_limit: 0,
            max_fee_per_gas: 0,
            max_priority_fee_per_gas: DEFAULT_PRIO_FEE,
            to: Default::default(),
            value: Default::default(),
            access_list: Default::default(),
            input: Default::default(),
            bundle_uuid_id: 0,
        }
    }
}
