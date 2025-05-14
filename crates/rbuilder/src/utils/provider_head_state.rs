//! Mod for gathering info about reth's head.

use crate::provider::StateProviderFactory;
use alloy_primitives::{BlockNumber, B256};
use reth_errors::ProviderResult;

/// For debugging. Results of asking for block number + hash to a BlockHashReader+BlockNumReader
#[derive(Debug)]
pub struct ProviderHeadStateBlockHash {
    /// Result of getting some (last/best) block number from the BlockNumReader
    pub block_number: ProviderResult<BlockNumber>,
    /// If block_number.is_ok -> Some(BlockHashReader::block_hash(block_number))
    pub block_hash: Option<ProviderResult<Option<B256>>>,
}

impl ProviderHeadStateBlockHash {
    pub fn new<P: StateProviderFactory>(
        provider: &P,
        block_number: ProviderResult<BlockNumber>,
    ) -> Self {
        Self {
            block_number: block_number.clone(),
            block_hash: block_number.map_or(None, |b| Some(provider.block_hash(b))),
        }
    }
}

/// For debugging. Results of asking to the StateProviderFactory info about last_block and best block.
#[derive(Debug)]
pub struct ProviderHeadState {
    pub last_block: ProviderHeadStateBlockHash,
    pub best_block: ProviderHeadStateBlockHash,
}

impl ProviderHeadState {
    pub fn new<P: StateProviderFactory>(provider: &P) -> Self {
        Self {
            last_block: ProviderHeadStateBlockHash::new(provider, provider.last_block_number()),
            best_block: ProviderHeadStateBlockHash::new(provider, provider.best_block_number()),
        }
    }
}
