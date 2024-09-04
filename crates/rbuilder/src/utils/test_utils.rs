use crate::primitives::{OrderId, TransactionSignedEcRecoveredWithBlobs};
use alloy_primitives::{Address, B256, I256, U256};
use reth_primitives::{TransactionSigned, TransactionSignedEcRecovered};

pub fn order_id(id: u64) -> OrderId {
    OrderId::Tx(hash(id))
}

pub fn hash(id: u64) -> B256 {
    B256::from(U256::from(id))
}

pub fn addr(id: u64) -> Address {
    Address::from_slice(&u256(id).as_le_slice()[0..20])
}

pub fn u256(i: u64) -> U256 {
    U256::from(i)
}

pub fn i256(i: i64) -> I256 {
    I256::try_from(i).unwrap()
}

pub fn tx(tx_hash: u64) -> TransactionSignedEcRecoveredWithBlobs {
    TransactionSignedEcRecoveredWithBlobs::new_for_testing(
        TransactionSignedEcRecovered::from_signed_transaction(
            TransactionSigned {
                hash: hash(tx_hash),
                signature: Default::default(),
                transaction: Default::default(),
            },
            Address::default(),
        ),
    )
}
