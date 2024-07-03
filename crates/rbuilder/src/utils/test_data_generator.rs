use alloy_primitives::{Address, BlockHash, TxHash, U256};
use uuid::Uuid;

/// TestDataGenerator allows you to create unique test objects with unique content, it tries to use different numbers for every field it sets since it may help debugging.
/// The idea is that each module creates its own TestDataGenerator that creates specific data needed in each context.
/// Ideally all other TestDataGenerators will contain one instance of this one for the basic stuff, ideally if several TestDataGenerators are combined the should share this TestDataGenerator
/// to guaranty no repeated data.
/// ALL generated data is based on a single u64 that increments on every use.
/// @Pending factorize with crates/rbuilder/src/mev_boost/rpc.rs (right now we are working on bidding so may generate conflicts)
#[derive(Default)]
pub struct TestDataGenerator {
    last_used_id: u64,
}

impl TestDataGenerator {
    pub fn create_u64(&mut self) -> u64 {
        self.last_used_id += 1;
        self.last_used_id
    }

    pub fn create_u256(&mut self) -> U256 {
        U256::from(self.create_u64())
    }

    pub fn create_uuid(&mut self) -> Uuid {
        Uuid::from_u128(self.create_u128())
    }

    pub fn create_u128(&mut self) -> u128 {
        self.create_u64() as u128
    }

    pub fn create_u8(&mut self) -> u8 {
        self.create_u64() as u8
    }

    pub fn create_address(&mut self) -> Address {
        Address::repeat_byte(self.create_u8())
    }

    pub fn create_block_hash(&mut self) -> BlockHash {
        BlockHash::from(self.create_u256())
    }

    pub fn create_tx_hash(&mut self) -> TxHash {
        TxHash::from(self.create_u256())
    }
}
