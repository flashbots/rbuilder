use std::time::{Duration, Instant};

use alloy_primitives::U256;
use reth::tasks::pool::BlockingTaskPool;
use reth_db::database::Database;
use reth_payload_builder::database::CachedReads;
use reth_primitives::format_ether;
use reth_provider::ProviderFactory;
use time::OffsetDateTime;
use tracing::trace;

use crate::{
    building::{
        estimate_payout_gas_limit, tracers::GasUsedSimulationTracer, BlockBuildingContext,
        BlockState, BuiltBlockTrace, BuiltBlockTraceError, CriticalCommitOrderError,
        EstimatePayoutGasErr, ExecutionError, ExecutionResult, FinalizeResult, PartialBlock,
        Sorting,
    },
    live_builder::bidding::SlotBidder,
    primitives::SimulatedOrder,
    roothash::RootHashMode,
    telemetry,
};

use super::{finalize_block_execution, Block};

/// Struct to help building blocks. It still needs to be finished by setting the payout tx and computing some extra stuff (eg: root hash).
/// Txs can still be added before finishing it.
/// Typical usage:
/// 1 - BlockBuildingHelper::new
/// 2 - Call lots of commit_order
/// 3 - Call set_trace_fill_time when you are done calling commit_order (we still have to review this step)
/// 4 - Call finalize_block.
pub struct BlockBuildingHelper<DB> {
    /// Balance of fee recipient before we stared building.
    /// Used to compute
    fee_recipient_balance_start: U256,
    /// Accumulated changes for the block (due to commit_order calls).
    block_state: BlockState,
    partial_block: PartialBlock<GasUsedSimulationTracer>,
    /// Gas reserved for the final payout txs from coinbase to fee recipient.
    /// None means we don't need this final tx since coinbase == fee recipient.
    payout_tx_gas: Option<u64>,
    /// Name of the builder that pregenerated this block.
    /// Might be ambiguous if several building parts were involved...
    builder_name: String,
    building_ctx: BlockBuildingContext,
    built_block_trace: BuiltBlockTrace,
    /// Needed to get the initial state and the final root hash calculation.
    provider_factory: ProviderFactory<DB>,
}

#[derive(Debug, thiserror::Error)]
pub enum BlockBuildingHelperError {
    #[error("Error accessing block data: {0}")]
    ProviderError(#[from] reth_errors::ProviderError),
    #[error("Unable estimate payout gas: {0}")]
    UnableToEstimatePayoutGas(#[from] EstimatePayoutGasErr),
    #[error("pre_block_call failed")]
    PreBlockCallFailed,
    #[error("InsertPayoutTxErr while finishing block: {0}")]
    InsertPayoutTxErr(#[from] crate::building::InsertPayoutTxErr),
    #[error("Bundle consistency check failed: {0}")]
    BundleConsistencyCheckFailed(#[from] BuiltBlockTraceError),
    #[error("Error finalizing block (eg:root hash): {0}")]
    FinalizeError(#[from] eyre::Report),
}

pub struct FinalizeBlockResult {
    /// None means the slot bidder told us to skip (probably this will soon change).
    pub block: Option<Block>,
    /// Since finalize_block eats the object we need the cached_reads in case we create a new
    pub cached_reads: CachedReads,
}

impl<DB: Database + Clone + 'static> BlockBuildingHelper<DB> {
    /// allow_tx_skip: see [`PartialBlockFork`]
    /// Performs initialization:
    /// - Query fee_recipient_balance_start.
    /// - pre_block_call.
    /// - Estimate payout tx cost.
    pub fn new(
        provider_factory: ProviderFactory<DB>,
        building_ctx: BlockBuildingContext,
        cached_reads: Option<CachedReads>,
        builder_name: String,
        discard_txs: bool,
        enforce_sorting: Option<Sorting>,
    ) -> Result<Self, BlockBuildingHelperError> {
        // @Maybe an issue - we have 2 db txs here (one for hash and one for finalize)
        let state_provider =
            provider_factory.history_by_block_hash(building_ctx.attributes.parent)?;
        let fee_recipient_balance_start = state_provider
            .account_balance(building_ctx.attributes.suggested_fee_recipient)?
            .unwrap_or_default();
        let mut partial_block = PartialBlock::new(discard_txs, enforce_sorting)
            .with_tracer(GasUsedSimulationTracer::default());
        let mut block_state =
            BlockState::new(state_provider).with_cached_reads(cached_reads.unwrap_or_default());
        partial_block
            .pre_block_call(&building_ctx, &mut block_state)
            .map_err(|_| BlockBuildingHelperError::PreBlockCallFailed)?;
        let payout_tx_gas = if building_ctx.coinbase_is_suggested_fee_recipient() {
            None
        } else {
            let payout_tx_gas = estimate_payout_gas_limit(
                building_ctx.attributes.suggested_fee_recipient,
                &building_ctx,
                &mut block_state,
                0,
            )?;
            partial_block.reserve_gas(payout_tx_gas);
            Some(payout_tx_gas)
        };
        Ok(Self {
            fee_recipient_balance_start,
            block_state,
            partial_block,
            payout_tx_gas,
            builder_name,
            building_ctx,
            built_block_trace: BuiltBlockTrace::new(),
            provider_factory,
        })
    }

    /// Forwards to partial_block and updates trace.
    pub fn commit_order(
        &mut self,
        order: &SimulatedOrder,
    ) -> Result<Result<&ExecutionResult, ExecutionError>, CriticalCommitOrderError> {
        let result =
            self.partial_block
                .commit_order(order, &self.building_ctx, &mut self.block_state);
        match result {
            Ok(ok_result) => match ok_result {
                Ok(res) => {
                    self.built_block_trace.add_included_order(res);
                    Ok(Ok(self.built_block_trace.included_orders.last().unwrap()))
                }
                Err(err) => {
                    self.built_block_trace
                        .modify_payment_when_no_signer_error(&err);
                    Ok(Err(err))
                }
            },
            Err(e) => Err(e),
        }
    }

    pub fn set_trace_fill_time(&mut self, time: Duration) {
        self.built_block_trace.fill_time = time;
    }

    /// Eats the object. At some point we might make a version that doesn't so we can call finalize_block several times.
    pub fn finalize_block(
        mut self,
        bidder: &dyn SlotBidder,
        orders_closed_at: OffsetDateTime,
        root_hash_task_pool: BlockingTaskPool,
        root_hash_mode: RootHashMode,
    ) -> Result<FinalizeBlockResult, BlockBuildingHelperError> {
        let start_time = Instant::now();
        let fee_recipient_balance_after = self
            .block_state
            .state_provider()
            .account_balance(self.building_ctx.attributes.suggested_fee_recipient)?
            .unwrap_or_default();

        let fee_recipient_balance_diff = fee_recipient_balance_after
            .checked_sub(self.fee_recipient_balance_start)
            .unwrap_or_default();

        let should_finalize = finalize_block_execution(
            &self.building_ctx,
            &mut self.partial_block,
            &mut self.block_state,
            &mut self.built_block_trace,
            self.payout_tx_gas,
            bidder,
            fee_recipient_balance_diff,
        )?;

        if !should_finalize {
            trace!(
                block = self.building_ctx.block_env.number.to::<u64>(),
                builder_name = self.builder_name,
                use_suggested_fee_recipient_as_coinbase =
                    self.building_ctx.coinbase_is_suggested_fee_recipient(),
                "Skipped block finalization",
            );
            let (cached_reads, _, _) = self.block_state.into_parts();
            return Ok(FinalizeBlockResult {
                block: None,
                cached_reads,
            });
        }

        // This could be moved outside of this func since I don´t think the payout tx can change much.
        self.built_block_trace
            .verify_bundle_consistency(&self.building_ctx.blocklist)?;

        let sim_gas_used = self.partial_block.tracer.used_gas;
        let finalized_block = self.partial_block.finalize(
            &mut self.block_state,
            &self.building_ctx,
            self.provider_factory.clone(),
            root_hash_mode,
            root_hash_task_pool,
        )?;
        self.built_block_trace
            .update_orders_timestamps_after_block_sealed(orders_closed_at);

        self.built_block_trace.finalize_time = start_time.elapsed();

        Self::trace_finalized_block(
            &finalized_block,
            &self.builder_name,
            &self.building_ctx,
            &self.built_block_trace,
            sim_gas_used,
        );

        let block = Block {
            trace: self.built_block_trace,
            sealed_block: finalized_block.sealed_block,
            txs_blobs_sidecars: finalized_block.txs_blob_sidecars,
            builder_name: self.builder_name.clone(),
        };
        Ok(FinalizeBlockResult {
            block: Some(block),
            cached_reads: finalized_block.cached_reads,
        })
    }

    /// Trace and telemetry
    fn trace_finalized_block(
        finalized_block: &FinalizeResult,
        builder_name: &String,
        building_ctx: &BlockBuildingContext,
        built_block_trace: &BuiltBlockTrace,
        sim_gas_used: u64,
    ) {
        let txs = finalized_block.sealed_block.body.len();
        let gas_used = finalized_block.sealed_block.gas_used;
        let blobs = finalized_block.txs_blob_sidecars.len();

        telemetry::add_built_block_metrics(
            built_block_trace.fill_time,
            built_block_trace.finalize_time,
            txs,
            blobs,
            gas_used,
            sim_gas_used,
            builder_name,
            building_ctx.timestamp(),
        );

        trace!(
            block = building_ctx.block_env.number.to::<u64>(),
            build_time_mus = built_block_trace.fill_time.as_micros(),
            finalize_time_mus = built_block_trace.finalize_time.as_micros(),
            profit = format_ether(built_block_trace.bid_value),
            builder_name = builder_name,
            txs,
            blobs,
            gas_used,
            sim_gas_used,
            use_suggested_fee_recipient_as_coinbase =
                building_ctx.coinbase_is_suggested_fee_recipient(),
            "Built block",
        );
    }
}
