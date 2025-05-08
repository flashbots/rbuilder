//! Heavily influenced by [reth](https://github.com/paradigmxyz/reth/blob/1e965caf5fa176f244a31c0d2662ba1b590938db/crates/optimism/payload/src/builder.rs#L570)
use alloy_consensus::Transaction;
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, TxHash, U256};
use reth_node_api::NodePrimitives;
use reth_optimism_primitives::OpReceipt;
use std::collections::HashSet;

/// Holds the state after execution
#[derive(Debug)]
pub struct ExecutedPayload<N: NodePrimitives> {
    /// Tracked execution info
    pub info: ExecutionInfo<N>,
}

#[derive(Default, Debug)]
pub struct ExecutionInfo<N: NodePrimitives> {
    /// All executed transactions (unrecovered).
    pub executed_transactions: Vec<N::SignedTx>,
    /// The recovered senders for the executed transactions.
    pub executed_senders: Vec<Address>,
    /// The transaction receipts
    pub receipts: Vec<OpReceipt>,
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

impl<N: NodePrimitives> ExecutionInfo<N> {
    /// Create a new instance with allocated slots.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            executed_transactions: Vec::with_capacity(capacity),
            executed_senders: Vec::with_capacity(capacity),
            receipts: Vec::with_capacity(capacity),
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
        tx: &N::SignedTx,
        block_gas_limit: u64,
        tx_data_limit: Option<u64>,
        block_data_limit: Option<u64>,
    ) -> bool {
        if self.cumulative_gas_used + tx.gas_limit() > block_gas_limit {
            return true;
        }

        if tx_data_limit.is_none() && block_data_limit.is_none() {
            return false;
        }

        let tx_compressed_size = op_alloy_flz::flz_compress_len(tx.encoded_2718().as_slice());
        if tx_data_limit.is_some_and(|da_limit| tx_compressed_size as u64 > da_limit) {
            return true;
        }

        if block_data_limit.is_some_and(|da_limit| {
            self.cumulative_da_bytes_used + (tx_compressed_size as u64) > da_limit
        }) {
            return true;
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_eips::Encodable2718;
    use alloy_primitives::private::alloy_rlp::Encodable;
    use alloy_primitives::Signature;
    use rand::RngCore;
    use reth_optimism_primitives::{OpPrimitives, OpTransactionSigned};

    #[test]
    fn test_block_gas_limit() {
        let gas_limit = 100;
        let info = ExecutionInfo::<OpPrimitives>::with_capacity(10);

        let allowable_transaction: OpTransactionSigned = TxEip1559 {
            gas_limit: gas_limit - 10,
            ..TxEip1559::default()
        }
        .into_signed(Signature::test_signature())
        .into();

        assert_eq!(
            false,
            info.is_tx_over_limits(&allowable_transaction, gas_limit, None, None)
        );

        let too_much_gas: OpTransactionSigned = TxEip1559 {
            gas_limit: gas_limit + 10,
            ..TxEip1559::default()
        }
        .into_signed(Signature::test_signature())
        .into();

        assert_eq!(
            true,
            info.is_tx_over_limits(&too_much_gas, gas_limit, None, None)
        );
    }

    fn gen_random_bytes(size: usize) -> alloy_primitives::bytes::Bytes {
        let mut rng = rand::thread_rng();
        let mut vec = vec![0u8; size];
        rng.fill_bytes(&mut vec);
        vec.into()
    }

    #[test]
    fn test_tx_da_size() {
        let allowable_transaction: OpTransactionSigned = TxEip1559 {
            ..TxEip1559::default()
        }
        .into_signed(Signature::test_signature())
        .into();

        let over_tx_da_limits: OpTransactionSigned = TxEip1559 {
            input: gen_random_bytes(2000).into(),
            ..TxEip1559::default()
        }
        .into_signed(Signature::test_signature())
        .into();

        let large_but_compressable: OpTransactionSigned = TxEip1559 {
            input: vec![0u8; 2000].into(),
            ..TxEip1559::default()
        }
        .into_signed(Signature::test_signature())
        .into();

        let max_tx_size: u64 = 1000;

        // Sanity check compressed and uncompressed transaction sizes
        // Uncompressed, the large transactions are the same size, but the all zero one compresses
        // 90+% and should be included when DA throttling is active
        assert_eq!(
            78,
            op_alloy_flz::flz_compress_len(allowable_transaction.encoded_2718().as_slice())
        );
        assert_eq!(81, allowable_transaction.length());
        assert_eq!(
            108,
            op_alloy_flz::flz_compress_len(large_but_compressable.encoded_2718().as_slice())
        );
        assert_eq!(2085, large_but_compressable.length());
        // Relative check as the randomness of the data may change the compressed size
        assert_eq!(
            true,
            max_tx_size
                < op_alloy_flz::flz_compress_len(over_tx_da_limits.encoded_2718().as_slice())
                    as u64
        );
        assert_eq!(2085, over_tx_da_limits.length());

        let info = ExecutionInfo::<OpPrimitives>::with_capacity(10);

        assert_eq!(
            false,
            info.is_tx_over_limits(&allowable_transaction, 10000, Some(max_tx_size), None)
        );
        assert_eq!(
            false,
            info.is_tx_over_limits(&large_but_compressable, 10000, Some(max_tx_size), None)
        );
        assert_eq!(
            true,
            info.is_tx_over_limits(&over_tx_da_limits, 10000, Some(max_tx_size), None)
        );

        // When no DA specific limits are set, large transactions are allowable
        assert_eq!(
            false,
            info.is_tx_over_limits(&over_tx_da_limits, 10000, None, None)
        );
    }

    #[test]
    fn test_block_da_limit() {
        let block_data_limit = 1000;
        let mut info = ExecutionInfo::<OpPrimitives>::with_capacity(10);

        let large_transaction: OpTransactionSigned = TxEip1559 {
            input: gen_random_bytes(2000).into(),
            ..TxEip1559::default()
        }
        .into_signed(Signature::test_signature())
        .into();

        let large_but_compressable: OpTransactionSigned = TxEip1559 {
            input: vec![0u8; 2000].into(),
            ..TxEip1559::default()
        }
        .into_signed(Signature::test_signature())
        .into();

        // Sanity check compressed and uncompressed transaction sizes
        assert_eq!(
            108,
            op_alloy_flz::flz_compress_len(large_but_compressable.encoded_2718().as_slice())
        );
        assert_eq!(2085, large_but_compressable.length());
        // Relative check as the randomness of the data may change the compressed size
        assert_eq!(
            true,
            block_data_limit
                < op_alloy_flz::flz_compress_len(large_transaction.encoded_2718().as_slice())
                    as u64
        );
        assert_eq!(2085, large_transaction.length());

        assert_eq!(
            true,
            info.is_tx_over_limits(&large_transaction, 1000, None, Some(block_data_limit))
        );
        assert_eq!(
            false,
            info.is_tx_over_limits(&large_but_compressable, 1000, None, Some(block_data_limit))
        );

        // Block level DA inclusion should take into account the current amount of DA bytes used in the block
        info.cumulative_da_bytes_used += 990;
        assert_eq!(
            true,
            info.is_tx_over_limits(&large_but_compressable, 1000, None, Some(block_data_limit))
        );
    }
}
