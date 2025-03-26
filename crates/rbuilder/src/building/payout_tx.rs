use super::{BlockBuildingContext, BlockState};
use crate::utils::Signer;
use alloy_consensus::{constants::KECCAK_EMPTY, TxEip1559};
use alloy_primitives::{Address, TxKind as TransactionKind, U256};
use reth_chainspec::ChainSpec;
use reth_errors::ProviderError;
use reth_evm::{Evm, EvmFactory};
use reth_primitives::{Recovered, Transaction, TransactionSigned};
use revm::context::result::{EVMError, ExecutionResult};

pub fn create_payout_tx(
    chain_spec: &ChainSpec,
    basefee: u64,
    signer: &Signer,
    nonce: u64,
    to: Address,
    gas_limit: u64,
    value: u128,
) -> Result<Recovered<TransactionSigned>, secp256k1::Error> {
    let tx = Transaction::Eip1559(TxEip1559 {
        chain_id: chain_spec.chain.id(),
        nonce,
        gas_limit,
        max_fee_per_gas: basefee as u128,
        max_priority_fee_per_gas: 0,
        to: TransactionKind::Call(to),
        value: U256::from(value),
        ..Default::default()
    });

    signer.sign_tx(tx)
}

#[derive(Debug, thiserror::Error)]
pub enum PayoutTxErr {
    #[error("Reth error: {0}")]
    Reth(#[from] ProviderError),
    #[error("Signature error: {0}")]
    SignError(#[from] secp256k1::Error),
    #[error("EVM error: {0}")]
    EvmError(#[from] EVMError<ProviderError>),
    #[error("Payout without signer")]
    NoSigner,
}

pub fn insert_test_payout_tx(
    to: Address,
    ctx: &BlockBuildingContext,
    state: &mut BlockState,
    gas_limit: u64,
) -> Result<Option<u64>, PayoutTxErr> {
    let builder_signer = ctx.builder_signer.as_ref().ok_or(PayoutTxErr::NoSigner)?;

    let nonce = state.nonce(builder_signer.address)?;

    let mut cfg = ctx.evm_env.cfg_env.clone();
    // disable balance check so we can estimate the gas cost without having any funds
    cfg.disable_balance_check = true;

    let tx = create_payout_tx(
        ctx.chain_spec.as_ref(),
        ctx.evm_env.block_env.basefee,
        builder_signer,
        nonce,
        to,
        gas_limit,
        0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF,
    )?;

    let mut db = state.new_db_ref();
    let mut evm = ctx.evm_factory.create_evm(db.as_mut(), ctx.evm_env.clone());
    let res = evm.transact(&tx)?;
    match res.result {
        ExecutionResult::Success {
            gas_used,
            gas_refunded,
            ..
        } => Ok(Some(gas_used + gas_refunded)),
        _ => Ok(None),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EstimatePayoutGasErr {
    #[error("Reth error: {0}")]
    Reth(#[from] ProviderError),
    #[error("Payout tx error: {0}")]
    PayoutTxErr(#[from] PayoutTxErr),
    #[error("Failed to estimate gas limit")]
    FailedToEstimate,
}
pub fn estimate_payout_gas_limit(
    to: Address,
    ctx: &BlockBuildingContext,
    state: &mut BlockState,
    gas_used: u64,
) -> Result<u64, EstimatePayoutGasErr> {
    tracing::trace!(address = ?to, "Estimating payout gas");
    if state.code_hash(to)? == KECCAK_EMPTY {
        return Ok(21_000);
    }

    let gas_left = ctx
        .evm_env
        .block_env
        .gas_limit
        .checked_sub(gas_used)
        .unwrap_or_default();
    let estimation = insert_test_payout_tx(to, ctx, state, gas_left)?
        .ok_or(EstimatePayoutGasErr::FailedToEstimate)?;

    if insert_test_payout_tx(to, ctx, state, estimation)?.is_some() {
        return Ok(estimation);
    }

    let mut left = estimation;
    let mut right = gas_left;

    // binary search for perfect gas limit
    loop {
        let mid = (left + right) / 2;
        if mid == left || mid == right {
            return Ok(right);
        }

        if insert_test_payout_tx(to, ctx, state, mid)?.is_some() {
            right = mid;
        } else {
            left = mid;
        }
    }
}
