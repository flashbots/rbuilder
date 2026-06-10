//! Arc-specific block building rules.
//!
//! Mirrors what `ArcBlockExecutor`/`ArcBlockAssembler` (arc-node) do around
//! transaction execution so that blocks built by rbuilder are byte-identical
//! to blocks built by the arc-node payload builder:
//!
//! * pre-block (Zero5+): EIP-2935 blockhashes system call.
//!   (arc does NOT do the EIP-4788 beacon root call — there is no beacon chain)
//! * post-block: read fee params from the ProtocolConfig contract, compute the
//!   next block base fee (EMA-smoothed, ADR-0004), persist the gas values into
//!   the SystemAccounting contract storage (a real state write that is part of
//!   the block state root) and encode the next base fee into the header
//!   `extra_data`.
//! * the block gas limit is dictated by ProtocolConfig (ADR-0003), not by the
//!   EIP-1559 gradual gas limit adjustment.

use crate::{
    building::{BlockBuildingContext, BlockState},
    chain,
};
use alloy_evm::Database;
use alloy_primitives::Bytes;
use arc_execution_config::{
    chainspec::{BaseFeeConfigProvider as _, BlockGasLimitProvider as _},
    gas_fee::{
        arc_calc_next_block_base_fee, determine_ema_parent_gas_used, encode_base_fee_to_bytes,
    },
    hardforks::{is_arc_fork_active, ArcHardfork},
    protocol_config,
};
use arc_precompiles::system_accounting::{self, GasValues};
use reth_errors::ProviderError;
use reth_evm::ConfigureEvm as _;
use revm::database::states::bundle_state::BundleRetention;
use std::sync::Arc;
use tracing::warn;

/// Computes the gas limit the next block must use, exactly as the arc-node
/// payload builder does (`ArcEvmConfig::builder_for_next_block`): query the
/// ProtocolConfig contract on top of the parent state and clamp to the
/// chainspec bounds.
///
/// `db` must be a database view of the parent block state.
pub fn expected_block_gas_limit<DB>(
    chain_spec: Arc<chain::ChainSpec>,
    db: DB,
    parent_header: &alloy_consensus::Header,
    evm_env: reth_evm::EvmEnv,
) -> u64
where
    DB: Database<Error: Send + Sync + std::error::Error + 'static> + revm::DatabaseCommit,
{
    let next_block_number = parent_header.number.saturating_add(1);
    let gas_limit_config = chain_spec.block_gas_limit_config(next_block_number);

    let evm_config = chain::evm_config(chain_spec);
    let mut evm = evm_config.evm_with_env(db, evm_env);
    let fee_params = protocol_config::retrieve_fee_params(&mut evm)
        .inspect_err(|err| {
            warn!(?err, "Failed to get fee params from ProtocolConfig, using default gas limit")
        })
        .ok();
    protocol_config::expected_gas_limit(fee_params.as_ref(), &gas_limit_config)
}

/// Arc pre-block system calls. Mirrors `ArcBlockExecutor::apply_pre_execution_changes`,
/// minus the validations that do not apply to the builder (we set the gas limit
/// and the beneficiary ourselves).
pub fn pre_block_calls<DB>(
    ctx: &BlockBuildingContext,
    state: &mut BlockState<DB>,
) -> eyre::Result<()>
where
    DB: Database<Error = ProviderError>,
{
    if !is_arc_fork_active(
        ctx.chain_spec.as_ref(),
        ArcHardfork::Zero5,
        ctx.block(),
        ctx.attributes.timestamp,
    ) {
        return Ok(());
    }

    let mut db = state.new_db_ref();
    let mut system_caller = alloy_evm::block::system_calls::SystemCaller::new(ctx.chain_spec.clone());
    let mut evm =
        chain::evm_config(ctx.chain_spec.clone()).evm_with_env(db.as_mut(), ctx.evm_env.clone());
    // EIP-2935: persist parent block hash in the history storage contract.
    // (internally gated on Prague activation, no-op at genesis)
    system_caller.apply_blockhashes_contract_call(ctx.attributes.parent, &mut evm)?;
    drop(evm);
    db.as_mut().merge_transitions(BundleRetention::Reverts);
    Ok(())
}

/// Result of the Arc post-block step.
pub struct ArcPostBlock {
    /// `extra_data` for the block header: the encoded next-block base fee.
    pub extra_data: Bytes,
}

/// Arc post-block step. Mirrors `ArcBlockExecutor::finish` + the extra_data
/// part of `ArcBlockAssembler::assemble_block`:
/// 1. read fee params from ProtocolConfig,
/// 2. compute the gas values / next base fee for this block,
/// 3. persist them into SystemAccounting storage (state write!),
/// 4. return the encoded next base fee for the header `extra_data`.
///
/// Must be called after all transactions (including the payout tx) are
/// committed, with `gas_used` being the total gas used by the block.
/// The caller is responsible for `merge_transitions` afterwards.
pub fn post_block_call<DB2>(
    db: &mut revm::database::State<DB2>,
    ctx: &BlockBuildingContext,
    gas_used: u64,
) -> eyre::Result<ArcPostBlock>
where
    DB2: Database<Error: Send + Sync + std::error::Error + 'static>,
{
    let chain_spec = ctx.chain_spec.as_ref();
    let block_number = ctx.block();
    let timestamp = ctx.attributes.timestamp;

    let mut evm =
        chain::evm_config(ctx.chain_spec.clone()).evm_with_env(db, ctx.evm_env.clone());

    let fee_params = protocol_config::retrieve_fee_params(&mut evm)
        .inspect_err(|err| {
            warn!(
                ?err,
                block_number, "Failed to retrieve fee params from ProtocolConfig"
            )
        })
        .ok();

    let zero5 = is_arc_fork_active(chain_spec, ArcHardfork::Zero5, block_number, timestamp);

    let gas_values = if zero5 {
        compute_gas_values(ctx, &mut evm, gas_used, fee_params)
            .map_err(|err| eyre::eyre!("Failed to compute Arc gas values: {err}"))?
    } else {
        compute_gas_values_legacy(ctx, &mut evm, gas_used, fee_params)
            .map_err(|err| eyre::eyre!("Failed to compute Arc gas values (legacy): {err}"))?
    };

    let extra_data = if zero5 && gas_values.nextBaseFee != 0 {
        encode_base_fee_to_bytes(gas_values.nextBaseFee)
    } else {
        // Matches ArcBlockAssembler: extra_data stays empty when there is no
        // next base fee to encode (pre-Zero5 / SystemAccounting unavailable).
        Bytes::default()
    };

    // Persist the gas values into SystemAccounting storage. This is a state
    // write and commits into the EVM db (part of the block's state root).
    system_accounting::store_gas_values(block_number, gas_values, &mut evm)
        .map_err(|err| eyre::eyre!("Failed to store Arc gas values: {err}"))?;

    Ok(ArcPostBlock { extra_data })
}

/// Mirrors `ArcBlockExecutor::compute_gas_values` (ADR-0004, Zero5+).
fn compute_gas_values<E>(
    ctx: &BlockBuildingContext,
    evm: &mut E,
    gas_used: u64,
    fee_params: Option<protocol_config::IProtocolConfig::FeeParams>,
) -> eyre::Result<GasValues>
where
    E: alloy_evm::Evm,
    E::DB: revm::DatabaseCommit,
{
    let chain_spec = ctx.chain_spec.as_ref();
    let block_number = ctx.block();

    if fee_params.is_none() {
        warn!(
            block_number,
            "ProtocolConfig unavailable post-Zero5; computing next_base_fee with chainspec defaults"
        );
    }

    let base_fee_config = chain_spec.base_fee_config(
        block_number
            .checked_add(1)
            .ok_or_else(|| eyre::eyre!("block number overflow"))?,
    );
    let calc = base_fee_config.resolve_calc_params(fee_params.as_ref());

    let parent_block_number = block_number.saturating_sub(1);
    let parent_gas_values = system_accounting::retrieve_gas_values(parent_block_number, evm)
        .map_err(|err| eyre::eyre!("Failed to retrieve parent gas values: {err}"))?;

    let smoothed_gas_used =
        determine_ema_parent_gas_used(parent_gas_values.gasUsedSmoothed, gas_used, calc.alpha)
            .unwrap_or(gas_used);

    let raw_next_base_fee = arc_calc_next_block_base_fee(
        smoothed_gas_used,
        gas_limit(ctx),
        base_fee(ctx),
        calc.k_rate,
        calc.inverse_elasticity_multiplier,
    );

    let clamped = match fee_params.as_ref() {
        Some(fp) => protocol_config::determine_bounded_base_fee(fp, raw_next_base_fee),
        None => raw_next_base_fee,
    };
    let next_base_fee = base_fee_config.clamp_absolute(clamped);

    Ok(GasValues {
        gasUsed: gas_used,
        gasUsedSmoothed: smoothed_gas_used,
        nextBaseFee: next_base_fee,
    })
}

/// Mirrors `ArcBlockExecutor::compute_gas_values_legacy` (pre-Zero5).
fn compute_gas_values_legacy<E>(
    ctx: &BlockBuildingContext,
    evm: &mut E,
    gas_used: u64,
    fee_params: Option<protocol_config::IProtocolConfig::FeeParams>,
) -> eyre::Result<GasValues>
where
    E: alloy_evm::Evm,
    E::DB: revm::DatabaseCommit,
{
    let Some(fee_params) = fee_params else {
        return Ok(GasValues {
            gasUsed: gas_used,
            gasUsedSmoothed: gas_used,
            nextBaseFee: 0,
        });
    };

    let block_number = ctx.block();
    let parent_block_number = block_number.saturating_sub(1);
    let parent_gas_values = system_accounting::retrieve_gas_values(parent_block_number, evm)
        .map_err(|err| eyre::eyre!("Failed to retrieve parent gas values: {err}"))?;

    let calculated_smoothed_gas_used = determine_ema_parent_gas_used(
        parent_gas_values.gasUsedSmoothed,
        gas_used,
        fee_params.alpha,
    );

    let mut next_base_fee: u64 = 0;
    if let Some(smoothed_gas_used) = calculated_smoothed_gas_used {
        let raw = arc_calc_next_block_base_fee(
            smoothed_gas_used,
            gas_limit(ctx),
            base_fee(ctx),
            fee_params.kRate,
            fee_params.inverseElasticityMultiplier,
        );
        next_base_fee = protocol_config::determine_bounded_base_fee(&fee_params, raw);
    }

    Ok(GasValues {
        gasUsed: gas_used,
        gasUsedSmoothed: calculated_smoothed_gas_used.unwrap_or(gas_used),
        nextBaseFee: next_base_fee,
    })
}

fn gas_limit(ctx: &BlockBuildingContext) -> u64 {
    ctx.evm_env.block_env.gas_limit
}

fn base_fee(ctx: &BlockBuildingContext) -> u64 {
    ctx.evm_env.block_env.basefee
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        building::{
            builders::mock_block_building_helper::MockRootHasher,
            evm::{create_chain_evm_factory, EvmFactory as _},
            BlockBuildingContext,
        },
        live_builder::order_input::mempool_txs_detector::MempoolTxsDetector,
        utils::Signer,
    };
    use alloy_primitives::{Address, U256};
    use alloy_rpc_types_beacon::events::{PayloadAttributesData, PayloadAttributesEvent};
    use alloy_rpc_types_engine::PayloadAttributes;
    use reth_chainspec::EthChainSpec as _;
    use revm::{
        context::TxEnv,
        database::{CacheDB, EmptyDB},
        state::AccountInfo,
    };
    use std::sync::Arc;

    #[test]
    fn parse_named_arc_chains() {
        for name in ["arc-localdev", "arc-devnet", "arc-testnet", "arc-mainnet"] {
            let spec = chain::parse_chain_spec(name).unwrap();
            assert!(spec.chain_id() != 0);
        }
    }

    /// The base fee of a new Arc block must come from the parent header's
    /// extra_data (where the executor of the parent block encoded it), and the
    /// gas limit must be exactly the suggested one (ProtocolConfig value), not
    /// an EIP-1559 gradual adjustment.
    #[test]
    fn block_env_from_arc_parent() {
        let chain_spec = chain::chain_spec_for_testing();
        let mut parent = chain::inner_chain_spec(&chain_spec)
            .sealed_genesis_header()
            .header()
            .clone();
        parent.extra_data = encode_base_fee_to_bytes(12_345);
        let parent_hash = parent.hash_slow();

        let attributes = PayloadAttributesEvent {
            version: "arc".to_string(),
            data: PayloadAttributesData {
                proposal_slot: 1,
                parent_block_root: Default::default(),
                parent_block_number: parent.number,
                parent_block_hash: parent_hash,
                proposer_index: 0,
                payload_attributes: PayloadAttributes {
                    timestamp: parent.timestamp + 1,
                    prev_randao: Default::default(),
                    suggested_fee_recipient: Address::random(),
                    withdrawals: Some(vec![]),
                    parent_beacon_block_root: Some(parent_hash),
                },
            },
        };

        let ctx = BlockBuildingContext::from_attributes(
            attributes,
            &parent,
            Signer::random(),
            chain_spec,
            Default::default(),
            Some(55_000_000),
            vec![],
            None,
            Arc::new(MockRootHasher {}),
            0,
            false,
            true,
            U256::ZERO,
            Default::default(),
            Arc::new(MempoolTxsDetector::new()),
        )
        .unwrap();

        assert_eq!(ctx.evm_env.block_env.basefee, 12_345);
        assert_eq!(ctx.evm_env.block_env.gas_limit, 55_000_000);
    }

    /// A plain value transfer through the rbuilder EVM factory must emit the
    /// Arc EIP-7708 native transfer log (proving order execution runs through
    /// the Arc EVM, not the stock Ethereum one).
    #[test]
    fn arc_evm_emits_native_transfer_log() {
        let chain_spec = chain::chain_spec_for_testing();
        let factory = create_chain_evm_factory(&chain_spec);

        let alice = Address::random();
        let bob = Address::random();
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            alice,
            AccountInfo {
                balance: U256::from(10).pow(U256::from(18)),
                ..Default::default()
            },
        );

        let evm_config = chain::evm_config(chain_spec.clone());
        let genesis = chain::inner_chain_spec(&chain_spec).sealed_genesis_header();
        let mut evm_env = evm_config.evm_env(genesis.header()).unwrap();
        evm_env.block_env.basefee = 0;
        evm_env.cfg_env.chain_id = chain_spec.chain_id();

        let mut evm = factory.create_evm(&mut db, evm_env);
        let result = alloy_evm::Evm::transact(
            &mut evm,
            TxEnv {
                caller: alice,
                kind: alloy_primitives::TxKind::Call(bob),
                value: U256::from(7),
                gas_limit: 21_000,
                gas_price: 0,
                chain_id: Some(chain_spec.chain_id()),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(result.result.is_success());
        let logs = result.result.logs();
        // EIP-7708 (Zero5+): plain value transfers emit an ERC-20 style
        // Transfer log from the EVM system address.
        let system_address = Address::from_slice(&alloy_primitives::hex!(
            "fffffffffffffffffffffffffffffffffffffffe"
        ));
        assert!(
            logs.iter().any(|log| log.address == system_address
                && log.data.data.as_ref().last() == Some(&7)),
            "expected EIP-7708 native transfer log, got: {logs:?}"
        );
    }
}
