use super::{evm::EvmFactory, BlockBuildingContext, BlockState, ThreadBlockBuildingContext};
use crate::{
    building::BlockSpace,
    utils::{constants::BASE_TX_GAS, Signer},
};
use alloy_consensus::{constants::KECCAK_EMPTY, TxEip1559};
use alloy_primitives::{Address, TxKind as TransactionKind, U256};
use alloy_rlp::Encodable as _;
use reth_chainspec::ChainSpec;
use reth_errors::ProviderError;
use reth_evm::Evm;
use reth_primitives::{Recovered, Transaction, TransactionSigned};
use revm::context::result::{EVMError, ExecutionResult};

pub fn create_payout_tx(
    chain_spec: &ChainSpec,
    basefee: u64,
    signer: &Signer,
    nonce: u64,
    to: Address,
    gas_limit: u64,
    value: U256,
) -> Result<Recovered<TransactionSigned>, secp256k1::Error> {
    let tx = Transaction::Eip1559(TxEip1559 {
        chain_id: chain_spec.chain.id(),
        nonce,
        gas_limit,
        max_fee_per_gas: basefee as u128,
        max_priority_fee_per_gas: 0,
        to: TransactionKind::Call(to),
        value,
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
}

impl PartialEq for PayoutTxErr {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (PayoutTxErr::Reth(_), PayoutTxErr::Reth(_)) => true,
            (PayoutTxErr::SignError(a), PayoutTxErr::SignError(b)) => a == b,
            (PayoutTxErr::EvmError(_), PayoutTxErr::EvmError(_)) => true,
            _ => false,
        }
    }
}

impl Eq for PayoutTxErr {}

pub fn insert_test_payout_tx(
    to: Address,
    ctx: &BlockBuildingContext,
    local_ctx: &mut ThreadBlockBuildingContext,
    state: &mut BlockState,
    gas_limit: u64,
) -> Result<Option<u64>, PayoutTxErr> {
    let builder_signer = &ctx.builder_signer;

    let nonce = state.nonce(
        builder_signer.address,
        &ctx.shared_cached_reads,
        &mut local_ctx.cached_reads,
    )?;

    let tx_value = 10u128.pow(18); // 10 ether
    let tx = create_payout_tx(
        ctx.chain_spec.as_ref(),
        ctx.evm_env.block_env.basefee,
        builder_signer,
        nonce,
        to,
        gas_limit,
        U256::from(tx_value),
    )?;
    let mut db = state.new_db_ref(&ctx.shared_cached_reads, &mut local_ctx.cached_reads);
    let mut evm = ctx.evm_factory.create_evm(db.as_mut(), ctx.evm_env.clone());

    let cache_account = evm.db_mut().load_cache_account(builder_signer.address)?;
    let gas_fee = ctx.evm_env.block_env.basefee as u128 * gas_limit as u128;
    cache_account.increment_balance((tx_value + gas_fee) * 2); // double for luck

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

impl PartialEq for EstimatePayoutGasErr {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (EstimatePayoutGasErr::Reth(_), EstimatePayoutGasErr::Reth(_)) => true,
            (EstimatePayoutGasErr::PayoutTxErr(a), EstimatePayoutGasErr::PayoutTxErr(b)) => a == b,
            (EstimatePayoutGasErr::FailedToEstimate, EstimatePayoutGasErr::FailedToEstimate) => {
                true
            }
            _ => false,
        }
    }
}

impl Eq for EstimatePayoutGasErr {}

fn estimate_payout_tx_space(ctx: &BlockBuildingContext) -> Result<BlockSpace, secp256k1::Error> {
    let gas_limit = ctx
        .evm_env
        .cfg_env
        .tx_gas_limit_cap
        .unwrap_or(ctx.evm_env.block_env.gas_limit);
    let tx = create_payout_tx(
        ctx.chain_spec.as_ref(),
        ctx.evm_env.block_env.basefee,
        &ctx.builder_signer,
        0,
        Address::ZERO,
        gas_limit,
        U256::ZERO,
    )?;
    Ok(BlockSpace::new(
        BASE_TX_GAS,
        tx.inner().length() + 32 * 4, /* To account for any possible length encoding on ZERO fields */
        0,
    ))
}

pub fn estimate_payout_gas_limit(
    to: Address,
    ctx: &BlockBuildingContext,
    local_ctx: &mut ThreadBlockBuildingContext,
    state: &mut BlockState,
    space_used: BlockSpace,
) -> Result<BlockSpace, EstimatePayoutGasErr> {
    tracing::trace!(address = ?to, "Estimating payout gas");
    // To simplify we compute the default payout tx rlp_length only once here. It's not worth computing the exact rlp_length for each estimation.
    let default_payout_tx_space =
        estimate_payout_tx_space(ctx).map_err(|_| EstimatePayoutGasErr::FailedToEstimate)?;
    if state.code_hash(to, &ctx.shared_cached_reads, &mut local_ctx.cached_reads)? == KECCAK_EMPTY {
        return Ok(default_payout_tx_space);
    }

    // We probably have a bug here since no reserved space was propagated but with a little bit of luck it will work because we call can_fit_tx later?
    let max_tx_gas_limit = ctx
        .evm_env
        .cfg_env
        .tx_gas_limit_cap
        .unwrap_or(ctx.evm_env.block_env.gas_limit);
    let gas_left = max_tx_gas_limit
        .checked_sub(space_used.gas)
        .unwrap_or_default();
    let estimation = insert_test_payout_tx(to, ctx, local_ctx, state, gas_left)?
        .ok_or(EstimatePayoutGasErr::FailedToEstimate)?;

    if insert_test_payout_tx(to, ctx, local_ctx, state, estimation)?.is_some() {
        return Ok(BlockSpace::new(
            estimation,
            default_payout_tx_space.rlp_length,
            0,
        ));
    }

    let mut left = estimation;
    let mut right = gas_left;

    // binary search for perfect gas limit
    loop {
        let mid = (left + right) / 2;
        if mid == left || mid == right {
            return Ok(BlockSpace::new(
                right,
                default_payout_tx_space.rlp_length,
                0,
            ));
        }

        if insert_test_payout_tx(to, ctx, local_ctx, state, mid)?.is_some() {
            right = mid;
        } else {
            left = mid;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::building::builders::mock_block_building_helper::MockRootHasher;
    use alloy_eips::eip1559::INITIAL_BASE_FEE;
    use alloy_primitives::B256;
    use assert_matches::assert_matches;
    use reth_chainspec::{EthereumHardfork, MAINNET};
    use reth_db::{tables, transaction::DbTxMut};
    use reth_primitives::Account;
    use reth_provider::test_utils::create_test_provider_factory_with_chain_spec;
    use revm::primitives::hardfork::SpecId;
    use std::sync::Arc;

    #[test]
    fn estimate_payout_tx_gas_limit() {
        let signer = Signer::random();
        let proposer = Address::random();
        let chain_spec = MAINNET.clone();
        let spec_id = SpecId::CANCUN;
        let cancun_timestamp = chain_spec
            .fork(EthereumHardfork::Cancun)
            .as_timestamp()
            .unwrap();

        // Insert proposer
        let provider_factory = create_test_provider_factory_with_chain_spec(chain_spec.clone());
        let provider_rw = provider_factory.provider_rw().unwrap();
        provider_rw
            .tx_ref()
            .put::<tables::PlainAccountState>(
                proposer,
                Account {
                    balance: U256::ZERO,
                    nonce: 1,
                    bytecode_hash: Some(B256::random()),
                },
            )
            .unwrap();
        provider_rw.commit().unwrap();

        let mut block: alloy_rpc_types::Block = Default::default();
        block.header.base_fee_per_gas = Some(INITIAL_BASE_FEE);
        block.header.timestamp = cancun_timestamp + 1;
        block.header.gas_limit = 30_000_000;
        let ctx = BlockBuildingContext::from_onchain_block(
            block,
            chain_spec,
            Some(spec_id),
            Default::default(),
            signer.address,
            proposer,
            signer,
            Arc::new(MockRootHasher {}),
            false,
            U256::ZERO,
        );
        let mut state = BlockState::new(provider_factory.latest().unwrap());
        let mut local_ctx = ThreadBlockBuildingContext::default();

        let estimate_result =
            estimate_payout_gas_limit(proposer, &ctx, &mut local_ctx, &mut state, BlockSpace::ZERO);
        assert_matches!(estimate_result, Ok(_));
        assert_eq!(estimate_result.unwrap().gas, 21_000);
    }
}
