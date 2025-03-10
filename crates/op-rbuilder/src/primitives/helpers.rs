use crate::tx_signer::Signer;
use alloy_consensus::transaction::Recovered;
use alloy_consensus::TxEip1559;
use alloy_primitives::{Address, TxKind};
use op_alloy_consensus::OpTypedTransaction;
use reth_evm::Database;
use reth_optimism_payload_builder::error::OpPayloadBuilderError;
use reth_optimism_primitives::OpTransactionSigned;
use reth_payload_primitives::PayloadBuilderError;
use reth_provider::ProviderError;
use revm::db::State;

/// Creates signed builder tx to Address::ZERO and specified message as input
pub fn signed_builder_tx<DB>(
    db: &mut State<DB>,
    builder_tx_gas: u64,
    message: Vec<u8>,
    signer: Signer,
    base_fee: u64,
    chain_id: u64,
) -> Result<Recovered<OpTransactionSigned>, PayloadBuilderError>
where
    DB: Database<Error = ProviderError>,
{
    // Create message with block number for the builder to sign
    let nonce = db
        .load_cache_account(signer.address)
        .map(|acc| acc.account_info().unwrap_or_default().nonce)
        .map_err(|_| {
            PayloadBuilderError::other(OpPayloadBuilderError::AccountLoadFailed(signer.address))
        })?;

    // Create the EIP-1559 transaction
    let tx = OpTypedTransaction::Eip1559(TxEip1559 {
        chain_id,
        nonce,
        gas_limit: builder_tx_gas,
        max_fee_per_gas: base_fee.into(),
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(Address::ZERO),
        // Include the message as part of the transaction data
        input: message.into(),
        ..Default::default()
    });
    // Sign the transaction
    let builder_tx = signer.sign_tx(tx).map_err(PayloadBuilderError::other)?;

    Ok(builder_tx)
}

pub fn estimate_gas_for_builder_tx(input: Vec<u8>) -> u64 {
    // Count zero and non-zero bytes
    let (zero_bytes, nonzero_bytes) = input.iter().fold((0, 0), |(zeros, nonzeros), &byte| {
        if byte == 0 {
            (zeros + 1, nonzeros)
        } else {
            (zeros, nonzeros + 1)
        }
    });

    // Calculate gas cost (4 gas per zero byte, 16 gas per non-zero byte)
    let zero_cost = zero_bytes * 4;
    let nonzero_cost = nonzero_bytes * 16;

    zero_cost + nonzero_cost + 21_000
}
