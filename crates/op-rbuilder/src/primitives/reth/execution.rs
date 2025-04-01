//! Heavily influenced by [reth](https://github.com/paradigmxyz/reth/blob/1e965caf5fa176f244a31c0d2662ba1b590938db/crates/optimism/payload/src/builder.rs#L570)
use alloy_consensus::Transaction;
use alloy_primitives::private::alloy_rlp::Encodable;
use alloy_primitives::{Address, TxHash, B256, U256};
use reth_node_api::NodePrimitives;
use revm::context::BlockEnv;
use std::collections::HashSet;

/// Holds the state after execution
#[derive(Debug)]
#[allow(dead_code)]
pub struct ExecutedPayload<N: NodePrimitives> {
    /// Tracked execution info
    pub info: ExecutionInfo,
    /// Withdrawal hash.
    pub withdrawals_root: Option<B256>,
    /// executed transactions
    pub executed_transactions: Vec<N::SignedTx>,
    /// executed senders
    pub executed_senders: Vec<Address>,
    /// The transaction receipts.
    pub receipts: Vec<N::Receipt>,
    /// The block env used during execution.
    pub block_env: BlockEnv,
}

impl<N: NodePrimitives> ExecutedPayload<N> {
    /// Create a new instance with allocated slots.
    #[allow(dead_code)]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            info: ExecutionInfo::new(),
            withdrawals_root: None,
            executed_transactions: Vec::with_capacity(capacity),
            executed_senders: Vec::with_capacity(capacity),
            receipts: Vec::with_capacity(capacity),
            block_env: BlockEnv::default(),
        }
    }
}
#[derive(Default, Debug)]
pub struct ExecutionInfo {
    /// All gas used so far
    pub cumulative_gas_used: u64,
    /// Estimated DA size
    pub cumulative_da_bytes_used: u64,
    /// Tracks fees from executed mempool transactions
    pub total_fees: U256,
    /// Tracks the reverted transaction hashes to remove from the transaction pool
    pub invalid_tx_hashes: HashSet<TxHash>,
    #[cfg(feature = "flashblocks")]
    /// Index of the last consumed flashblock
    pub last_flashblock_index: usize,
}

impl ExecutionInfo {
    /// Create a new instance with allocated slots.
    pub fn new() -> Self {
        Self {
            cumulative_gas_used: 0,
            cumulative_da_bytes_used: 0,
            total_fees: U256::ZERO,
            invalid_tx_hashes: HashSet::new(),
            #[cfg(feature = "flashblocks")]
            last_flashblock_index: 0,
        }
    }

    /// Returns true if the transaction would exceed the block limits:
    /// - block gas limit: ensures the transaction still fits into the block.
    /// - tx DA limit: if configured, ensures the tx does not exceed the maximum allowed DA limit
    ///   per tx.
    /// - block DA limit: if configured, ensures the transaction's DA size does not exceed the
    ///   maximum allowed DA limit per block.
    pub fn is_tx_over_limits(
        &self,
        tx: &(impl Encodable + Transaction),
        block_gas_limit: u64,
        tx_data_limit: Option<u64>,
        block_data_limit: Option<u64>,
    ) -> bool {
        if tx_data_limit.is_some_and(|da_limit| tx.length() as u64 > da_limit) {
            return true;
        }

        if block_data_limit
            .is_some_and(|da_limit| self.cumulative_da_bytes_used + (tx.length() as u64) > da_limit)
        {
            return true;
        }

        self.cumulative_gas_used + tx.gas_limit() > block_gas_limit
    }
}
