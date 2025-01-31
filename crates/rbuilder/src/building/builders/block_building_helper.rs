use alloy_primitives::{utils::format_ether, U256};
use reth::revm::cached::CachedReads;
use std::{
    cmp::max,
    time::{Duration, Instant},
};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, trace};

use crate::{
    building::{
        estimate_payout_gas_limit, tracers::GasUsedSimulationTracer, BlockBuildingContext,
        BlockState, BuiltBlockTrace, BuiltBlockTraceError, CriticalCommitOrderError,
        EstimatePayoutGasErr, ExecutionError, ExecutionResult, FinalizeError, FinalizeResult,
        PartialBlock, Sorting,
    },
    primitives::SimulatedOrder,
    provider::StateProviderFactory,
    telemetry::{self, add_block_fill_time, add_order_simulation_time},
    utils::{check_block_hash_reader_health, HistoricalBlockError},
};

use super::Block;

/// Trait to help building blocks. It still needs to be finished (finalize_block) to set the payout tx and computing some extra stuff (eg: root hash).
/// Txs can be added before finishing it.
/// Typical usage:
/// 1 - Create it some how.
/// 2 - Call lots of commit_order.
/// 3 - Call set_trace_fill_time when you are done calling commit_order (we still have to review this step).
/// 4 - Call finalize_block.
pub trait BlockBuildingHelper: Send + Sync {
    fn box_clone(&self) -> Box<dyn BlockBuildingHelper>;

    /// Tries to add an order to the end of the block.
    /// Block state changes only on Ok(Ok)
    fn commit_order(
        &mut self,
        order: &SimulatedOrder,
    ) -> Result<Result<&ExecutionResult, ExecutionError>, CriticalCommitOrderError>;

    /// Call set the trace fill_time (we still have to review this)
    fn set_trace_fill_time(&mut self, time: Duration);
    /// If not set the trace will default to creation time.
    fn set_trace_orders_closed_at(&mut self, orders_closed_at: OffsetDateTime);

    /// Only if can_add_payout_tx you can pass Some(payout_tx_value) to finalize_block (a little ugly could be improved...)
    fn can_add_payout_tx(&self) -> bool;

    /// Accumulated coinbase delta - gas cost of final payout tx (if can_add_payout_tx).
    /// This is the maximum profit that can reach the final fee recipient (max bid!).
    /// Maximum payout_tx_value value to pass to finalize_block.
    /// The main reason to get an error is if profit is so low that we can't pay the payout tx (that would mean negative block value!).
    fn true_block_value(&self) -> Result<U256, BlockBuildingHelperError>;

    /// Eats the BlockBuildingHelper since once it's finished you should not use it anymore.
    /// payout_tx_value: If Some, added at the end of the block from coinbase to the final fee recipient.
    ///     This only works if can_add_payout_tx.
    fn finalize_block(
        self: Box<Self>,
        payout_tx_value: Option<U256>,
    ) -> Result<FinalizeBlockResult, BlockBuildingHelperError>;

    /// Useful if we want to give away this object but keep on building some other way.
    fn clone_cached_reads(&self) -> CachedReads;

    /// BuiltBlockTrace for current state.
    fn built_block_trace(&self) -> &BuiltBlockTrace;

    /// BlockBuildingContext used for building.
    fn building_context(&self) -> &BlockBuildingContext;

    /// Updates the cached reads for the block state.
    fn update_cached_reads(&mut self, cached_reads: CachedReads);

    /// Name of the builder that pregenerated this block.
    /// BE CAREFUL: Might be ambiguous if several building parts were involved...
    fn builder_name(&self) -> &str;
}

/// Implementation of BlockBuildingHelper based on a generic Provider
#[derive(Clone)]
pub struct BlockBuildingHelperFromProvider<P>
where
    P: StateProviderFactory,
{
    /// Balance of fee recipient before we stared building.
    _fee_recipient_balance_start: U256,
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
    provider: P,
    /// Token to cancel in case of fatal error (if we believe that it's impossible to build for this block).
    cancel_on_fatal_error: CancellationToken,
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
    #[error("Error finalizing block: {0}")]
    FinalizeError(#[from] FinalizeError),
    #[error("Payout tx not allowed for block")]
    PayoutTxNotAllowed,
    #[error("Provider historical block hashes error: {0}")]
    HistoricalBlockError(#[from] HistoricalBlockError),
}

impl BlockBuildingHelperError {
    /// Non critial error can happen during normal operations of the builder
    pub fn is_critical(&self) -> bool {
        match self {
            BlockBuildingHelperError::FinalizeError(finalize) => {
                !finalize.is_consistent_db_view_err()
            }
            BlockBuildingHelperError::InsertPayoutTxErr(
                crate::building::InsertPayoutTxErr::ProfitTooLow,
            ) => false,
            _ => true,
        }
    }
}

pub struct FinalizeBlockResult {
    pub block: Block,
    /// Since finalize_block eats the object we need the cached_reads in case we create a new
    pub cached_reads: CachedReads,
}

impl<P> BlockBuildingHelperFromProvider<P>
where
    P: StateProviderFactory + Clone + 'static,
{
    /// allow_tx_skip: see [`PartialBlockFork`]
    /// Performs initialization:
    /// - Query fee_recipient_balance_start.
    /// - pre_block_call.
    /// - Estimate payout tx cost.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: P,
        building_ctx: BlockBuildingContext,
        cached_reads: Option<CachedReads>,
        builder_name: String,
        discard_txs: bool,
        enforce_sorting: Option<Sorting>,
        cancel_on_fatal_error: CancellationToken,
    ) -> Result<Self, BlockBuildingHelperError> {
        // @Maybe an issue - we have 2 db txs here (one for hash and one for finalize)
        let state_provider = provider.history_by_block_hash(building_ctx.attributes.parent)?;

        let last_committed_block = building_ctx.block() - 1;
        check_block_hash_reader_health(last_committed_block, &state_provider)?;

        let fee_recipient_balance_start = state_provider
            .account_balance(&building_ctx.attributes.suggested_fee_recipient)?
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
            _fee_recipient_balance_start: fee_recipient_balance_start,
            block_state,
            partial_block,
            payout_tx_gas,
            builder_name,
            building_ctx,
            built_block_trace: BuiltBlockTrace::new(),
            provider,
            cancel_on_fatal_error,
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
        let txs = finalized_block.sealed_block.body().transactions.len();
        let gas_used = finalized_block.sealed_block.gas_used;
        let blobs = finalized_block.txs_blob_sidecars.len();

        telemetry::add_finalized_block_metrics(
            built_block_trace,
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
            root_hash_time_mus = built_block_trace.root_hash_time.as_micros(),
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

    /// Inserts payout tx if necessary and updates built_block_trace.
    fn finalize_block_execution(
        &mut self,
        payout_tx_value: Option<U256>,
    ) -> Result<(), BlockBuildingHelperError> {
        self.built_block_trace.coinbase_reward = self.partial_block.coinbase_profit;
        let (bid_value, true_value) = if let (Some(payout_tx_gas), Some(payout_tx_value)) =
            (self.payout_tx_gas, payout_tx_value)
        {
            match self.partial_block.insert_proposer_payout_tx(
                payout_tx_gas,
                payout_tx_value,
                &self.building_ctx,
                &mut self.block_state,
            ) {
                Ok(()) => (payout_tx_value, self.true_block_value()?),
                Err(err) => return Err(err.into()),
            }
        } else {
            (
                self.partial_block.coinbase_profit,
                self.partial_block.coinbase_profit,
            )
        };
        // Since some extra money might arrived directly the suggested_fee_recipient (when suggested_fee_recipient != coinbase)
        // we check the fee_recipient delta and make our bid include that! This is supposed to be what the relay will check.
        let fee_recipient_balance_after = self
            .block_state
            .balance(self.building_ctx.attributes.suggested_fee_recipient)?;
        let fee_recipient_balance_diff = fee_recipient_balance_after
            .checked_sub(self._fee_recipient_balance_start)
            .unwrap_or_default();
        self.built_block_trace.bid_value = max(bid_value, fee_recipient_balance_diff);
        self.built_block_trace.true_bid_value = true_value;
        Ok(())
    }
}

impl<P> BlockBuildingHelper for BlockBuildingHelperFromProvider<P>
where
    P: StateProviderFactory + Clone + 'static,
{
    /// Forwards to partial_block and updates trace.
    fn commit_order(
        &mut self,
        order: &SimulatedOrder,
    ) -> Result<Result<&ExecutionResult, ExecutionError>, CriticalCommitOrderError> {
        let start = Instant::now();
        let result =
            self.partial_block
                .commit_order(order, &self.building_ctx, &mut self.block_state);
        let sim_time = start.elapsed();
        let (result, sim_ok) = match result {
            Ok(ok_result) => match ok_result {
                Ok(res) => {
                    self.built_block_trace.add_included_order(res);
                    (
                        Ok(Ok(self.built_block_trace.included_orders.last().unwrap())),
                        true,
                    )
                }
                Err(err) => {
                    self.built_block_trace
                        .modify_payment_when_no_signer_error(&err);
                    (Ok(Err(err)), false)
                }
            },
            Err(e) => (Err(e), false),
        };
        add_order_simulation_time(sim_time, &self.builder_name, sim_ok);
        result
    }

    fn set_trace_fill_time(&mut self, time: Duration) {
        self.built_block_trace.fill_time = time;
        add_block_fill_time(time, &self.builder_name, self.building_ctx.timestamp())
    }

    fn set_trace_orders_closed_at(&mut self, orders_closed_at: OffsetDateTime) {
        self.built_block_trace.orders_closed_at = orders_closed_at;
    }

    fn can_add_payout_tx(&self) -> bool {
        !self.building_ctx.coinbase_is_suggested_fee_recipient()
    }

    fn true_block_value(&self) -> Result<U256, BlockBuildingHelperError> {
        if let Some(payout_tx_gas) = self.payout_tx_gas {
            Ok(self
                .partial_block
                .get_proposer_payout_tx_value(payout_tx_gas, &self.building_ctx)?)
        } else {
            Ok(self.partial_block.coinbase_profit)
        }
    }

    fn finalize_block(
        mut self: Box<Self>,
        payout_tx_value: Option<U256>,
    ) -> Result<FinalizeBlockResult, BlockBuildingHelperError> {
        if payout_tx_value.is_some() && self.building_ctx.coinbase_is_suggested_fee_recipient() {
            return Err(BlockBuildingHelperError::PayoutTxNotAllowed);
        }
        let start_time = Instant::now();

        self.finalize_block_execution(payout_tx_value)?;
        // This could be moved outside of this func (pre finalize) since I don´t think the payout tx can change much.
        self.built_block_trace
            .verify_bundle_consistency(&self.building_ctx.blocklist)?;

        let sim_gas_used = self.partial_block.tracer.used_gas;
        let block_number = self.building_context().block();
        let finalized_block = match self
            .partial_block
            .finalize(&mut self.block_state, &self.building_ctx)
        {
            Ok(finalized_block) => finalized_block,
            Err(err) => {
                if err.is_consistent_db_view_err() {
                    let last_block_number = self.provider.last_block_number().unwrap_or_default();
                    debug!(
                        block_number,
                        last_block_number, "Can't build on this head, cancelling slot"
                    );
                    self.cancel_on_fatal_error.cancel();
                }
                return Err(BlockBuildingHelperError::FinalizeError(err));
            }
        };
        self.built_block_trace.update_orders_sealed_at();
        self.built_block_trace.root_hash_time = finalized_block.root_hash_time;
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
            execution_requests: finalized_block.execution_requests,
        };
        Ok(FinalizeBlockResult {
            block,
            cached_reads: finalized_block.cached_reads,
        })
    }

    fn clone_cached_reads(&self) -> CachedReads {
        self.block_state.clone_cached_reads()
    }

    fn built_block_trace(&self) -> &BuiltBlockTrace {
        &self.built_block_trace
    }

    fn building_context(&self) -> &BlockBuildingContext {
        &self.building_ctx
    }

    fn box_clone(&self) -> Box<dyn BlockBuildingHelper> {
        Box::new(self.clone())
    }

    fn update_cached_reads(&mut self, cached_reads: CachedReads) {
        self.block_state = self.block_state.clone().with_cached_reads(cached_reads);
    }

    fn builder_name(&self) -> &str {
        &self.builder_name
    }
}
