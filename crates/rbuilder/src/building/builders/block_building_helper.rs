use alloy_primitives::{utils::format_ether, Address, TxHash, I256, U256};
use reth_provider::StateProvider;
use std::{
    cmp::max,
    sync::Arc,
    time::{Duration, Instant},
};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, trace, warn};

use crate::{
    building::{
        builders::BuiltBlockId, estimate_payout_gas_limit, tracers::GasUsedSimulationTracer,
        BlockBuildingContext, BlockSpace, BlockState, BuiltBlockTrace, BuiltBlockTraceError,
        CriticalCommitOrderError, EstimatePayoutGasErr, ExecutionError, ExecutionResult,
        FinalizeAdjustmentState, FinalizeError, FinalizeResult,
        FinalizeRevertStateCurrentIteration, NullPartialBlockExecutionTracer, PartialBlock,
        PartialBlockExecutionTracer, ThreadBlockBuildingContext,
    },
    telemetry::{self, add_block_fill_time, add_order_simulation_time},
    utils::{check_block_hash_reader_health, elapsed_ms, HistoricalBlockError},
};
use rbuilder_primitives::{
    order_statistics::OrderStatistics, SimValue, SimulatedOrder,
    TransactionSignedEcRecoveredWithBlobs,
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
    /// See [PartialBlock::commit_order]
    fn commit_order(
        &mut self,
        local_ctx: &mut ThreadBlockBuildingContext,
        order: &SimulatedOrder,
        result_filter: &dyn Fn(&SimValue) -> Result<(), ExecutionError>,
    ) -> Result<Result<&ExecutionResult, ExecutionError>, CriticalCommitOrderError>;

    /// Call set the trace fill_time (we still have to review this)
    fn set_trace_fill_time(&mut self, time: Duration);
    /// If not set the trace will default to creation time.
    fn set_trace_orders_closed_at(&mut self, orders_closed_at: OffsetDateTime);

    fn set_filtered_build_statistics(
        &mut self,
        considered_orders_statistics: OrderStatistics,
        failed_orders_statistics: OrderStatistics,
    );

    /// Accumulated coinbase delta - gas cost of final payout tx (if can_add_payout_tx).
    /// This is the maximum profit that can reach the final fee recipient (max bid!).
    /// Maximum payout_tx_value value to pass to finalize_block.
    /// The main reason to get an error is if profit is so low that we can't pay the payout tx (that would mean negative block value!).
    fn true_block_value(&self) -> Result<U256, BlockBuildingHelperError>;

    /// Finalize block for submission.
    /// if adjust_finalized_block is implemented, finalize_blocks should prepare helper
    /// for faster adjustments.
    /// subsidy is how much of payout_tx_value we consider to be subsidy.
    fn finalize_block(
        &mut self,
        local_ctx: &mut ThreadBlockBuildingContext,
        payout_tx_value: U256,
        subsidy: I256,
        seen_competition_bid: Option<U256>,
    ) -> Result<FinalizeBlockResult, BlockBuildingHelperError>;

    /// BuiltBlockTrace for current state.
    fn built_block_trace(&self) -> &BuiltBlockTrace;

    fn id(&self) -> BuiltBlockId {
        self.built_block_trace().build_block_id
    }

    /// BlockBuildingContext used for building.
    fn building_context(&self) -> &BlockBuildingContext;

    /// Name of the builder that pregenerated this block.
    /// BE CAREFUL: Might be ambiguous if several building parts were involved...
    fn builder_name(&self) -> &str;

    /// adjust_finalized_block will be called on block that was previously finalize with
    /// finalize_block. local_ctx will be set to the one used for finalize_block call.
    /// This method is supposed to be faster than calling finalize_block from scratch.
    fn adjust_finalized_block(
        &mut self,
        local_ctx: &mut ThreadBlockBuildingContext,
        payout_tx_value: U256,
        subsidy: I256,
        seen_competition_bid: Option<U256>,
    ) -> Result<FinalizeBlockResult, BlockBuildingHelperError>;
}

/// Wraps a BlockBuildingHelper with a valid true_block_value which makes it ready to bid.
pub struct BiddableUnfinishedBlock {
    pub block: Box<dyn BlockBuildingHelper>,
    pub true_block_value: U256,
    pub chosen_as_best_at: OffsetDateTime,
}

impl Clone for BiddableUnfinishedBlock {
    fn clone(&self) -> Self {
        Self {
            block: self.block.box_clone(),
            true_block_value: self.true_block_value,
            chosen_as_best_at: self.chosen_as_best_at,
        }
    }
}

impl BiddableUnfinishedBlock {
    pub fn new(block: Box<dyn BlockBuildingHelper>) -> Result<Self, BlockBuildingHelperError> {
        let true_block_value = block.true_block_value()?;
        Ok(Self {
            block,
            true_block_value,
            chosen_as_best_at: OffsetDateTime::now_utc(),
        })
    }
    pub fn id(&self) -> BuiltBlockId {
        self.block.id()
    }

    pub fn block(&self) -> &dyn BlockBuildingHelper {
        self.block.as_ref()
    }

    pub fn into_building_helper(self) -> Box<dyn BlockBuildingHelper> {
        self.block
    }
}

/// Implementation of BlockBuildingHelper based on a generic Provider
#[derive(Clone)]
pub struct BlockBuildingHelperFromProvider<
    PartialBlockExecutionTracerType: PartialBlockExecutionTracer + Clone + Send + Sync + 'static,
> {
    /// Balance of fee recipient before we stared building.
    _fee_recipient_balance_start: U256,
    /// Accumulated changes for the block (due to commit_order calls).
    block_state: BlockState,
    partial_block: PartialBlock<GasUsedSimulationTracer, PartialBlockExecutionTracerType>,
    /// Gas reserved for the final payout txs from coinbase to fee recipient.
    payout_tx_gas: u64,
    /// Name of the builder that pregenerated this block.
    /// Might be ambiguous if several building parts were involved...
    builder_name: String,
    building_ctx: BlockBuildingContext,
    built_block_trace: BuiltBlockTrace,
    /// Token to cancel in case of fatal error (if we believe that it's impossible to build for this block).
    cancel_on_fatal_error: CancellationToken,

    finalize_adjustment_state: Option<FinalizeAdjustmentState>,

    /// If an order execution duration (commit_order) is greater than this, we will log a warning with some info about the order.
    /// This probably should not be implemented here and should be a wrapper but this is simpler.
    max_order_execution_duration_warning: Option<Duration>,
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
    #[error("Provider historical block hashes error: {0}")]
    HistoricalBlockError(#[from] HistoricalBlockError),
    #[error("Block is not finalized correctly")]
    BlockFinalizedIncorrectly,
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
}

impl BlockBuildingHelperFromProvider<NullPartialBlockExecutionTracer> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        built_block_id: BuiltBlockId,
        state_provider: Arc<dyn StateProvider>,
        building_ctx: BlockBuildingContext,
        local_ctx: &mut ThreadBlockBuildingContext,
        builder_name: String,
        discard_txs: bool,
        available_orders_statistics: OrderStatistics,
        cancel_on_fatal_error: CancellationToken,
        max_order_execution_duration_warning: Option<Duration>,
    ) -> Result<Self, BlockBuildingHelperError> {
        BlockBuildingHelperFromProvider::new_with_execution_tracer(
            built_block_id,
            state_provider,
            building_ctx,
            local_ctx,
            builder_name,
            discard_txs,
            available_orders_statistics,
            cancel_on_fatal_error,
            NullPartialBlockExecutionTracer {},
            max_order_execution_duration_warning,
        )
    }
}

impl<
        PartialBlockExecutionTracerType: PartialBlockExecutionTracer + Clone + Send + Sync + 'static,
    > BlockBuildingHelperFromProvider<PartialBlockExecutionTracerType>
{
    /// allow_tx_skip: see [`PartialBlockFork`]
    /// Performs initialization:
    /// - Query fee_recipient_balance_start.
    /// - pre_block_call.
    /// - Estimate payout tx cost.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_execution_tracer(
        built_block_id: BuiltBlockId,
        state_provider: Arc<dyn StateProvider>,
        building_ctx: BlockBuildingContext,
        local_ctx: &mut ThreadBlockBuildingContext,
        builder_name: String,
        discard_txs: bool,
        available_orders_statistics: OrderStatistics,
        cancel_on_fatal_error: CancellationToken,
        partial_block_execution_tracer: PartialBlockExecutionTracerType,
        max_order_execution_duration_warning: Option<Duration>,
    ) -> Result<Self, BlockBuildingHelperError> {
        let last_committed_block = building_ctx.block() - 1;
        check_block_hash_reader_health(last_committed_block, &state_provider)?;

        let fee_recipient_balance_start = state_provider
            .account_balance(&building_ctx.attributes.suggested_fee_recipient)?
            .unwrap_or_default();
        let mut partial_block =
            PartialBlock::new_with_execution_tracer(discard_txs, partial_block_execution_tracer)
                .with_tracer(GasUsedSimulationTracer::default());
        let mut block_state = BlockState::new_arc(state_provider);
        partial_block
            .pre_block_call(&building_ctx, local_ctx, &mut block_state)
            .map_err(|_| BlockBuildingHelperError::PreBlockCallFailed)?;
        let payout_tx_space = estimate_payout_gas_limit(
            building_ctx.attributes.suggested_fee_recipient,
            &building_ctx,
            local_ctx,
            &mut block_state,
            BlockSpace::ZERO,
        )?;
        partial_block.reserve_block_space(payout_tx_space);
        let payout_tx_gas = payout_tx_space.gas;

        let mut built_block_trace = BuiltBlockTrace::new(built_block_id);
        built_block_trace.available_orders_statistics = available_orders_statistics;
        Ok(Self {
            _fee_recipient_balance_start: fee_recipient_balance_start,
            block_state,
            partial_block,
            payout_tx_gas,
            builder_name,
            building_ctx,
            built_block_trace,
            cancel_on_fatal_error,
            finalize_adjustment_state: None,
            max_order_execution_duration_warning,
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
            block = building_ctx.block(),
            build_time_mus = built_block_trace.fill_time.as_micros(),
            finalize_time_mus = built_block_trace.finalize_time.as_micros(),
            finalize_adjust_time_mus = built_block_trace.finalize_adjust_time.as_micros(),
            root_hash_time_mus = built_block_trace.root_hash_time.as_micros(),
            profit = format_ether(built_block_trace.bid_value),
            builder_name = builder_name,
            txs,
            blobs,
            gas_used,
            sim_gas_used,
            "Built block",
        );
    }

    /// Inserts payout tx if necessary and updates built_block_trace.
    fn finalize_block_execution(
        &mut self,
        local_ctx: &mut ThreadBlockBuildingContext,
        payout_tx_value: U256,
        subsidy: I256,
        adjust_finalized_block: bool,
        finalize_revert_state: &mut FinalizeRevertStateCurrentIteration,
    ) -> Result<(), BlockBuildingHelperError> {
        self.built_block_trace.coinbase_reward = self.partial_block.coinbase_profit;

        self.partial_block.insert_refunds_and_proposer_payout_tx(
            self.payout_tx_gas,
            payout_tx_value,
            &self.building_ctx,
            local_ctx,
            &mut self.block_state,
            adjust_finalized_block,
            finalize_revert_state,
        )?;

        let (bid_value, true_value) = (payout_tx_value, self.true_block_value()?);

        let fee_recipient_balance_after = self.block_state.balance(
            self.building_ctx.attributes.suggested_fee_recipient,
            &self.building_ctx.shared_cached_reads,
            &mut local_ctx.cached_reads,
        )?;
        let fee_recipient_balance_diff = fee_recipient_balance_after
            .checked_sub(self._fee_recipient_balance_start)
            .unwrap_or_default();

        self.built_block_trace.bid_value = max(bid_value, fee_recipient_balance_diff);
        self.built_block_trace.subsidy = subsidy;
        self.built_block_trace.true_bid_value = true_value;
        self.built_block_trace.mev_blocker_price = self.building_context().mev_blocker_price;
        Ok(())
    }

    fn finalize_block_impl(
        &mut self,
        local_ctx: &mut ThreadBlockBuildingContext,
        payout_tx_value: U256,
        subsidy: I256,
        seen_competition_bid: Option<U256>,
        adjust_finalized_block: bool,
    ) -> Result<FinalizeBlockResult, BlockBuildingHelperError> {
        if adjust_finalized_block != self.finalize_adjustment_state.is_some() {
            return Err(BlockBuildingHelperError::BlockFinalizedIncorrectly);
        }

        let start_time = Instant::now();
        let step_start = Instant::now();

        let FinalizeAdjustmentState {
            revert_state,
            previous_finalize_data,
        } = self.finalize_adjustment_state.take().unwrap_or_default();

        if adjust_finalized_block {
            self.partial_block
                .adjust_finalize_block_revert_to_prefinalized_state(
                    revert_state,
                    &mut self.block_state,
                );
        }
        let mut finalize_adjustment_state = FinalizeAdjustmentState {
            revert_state: Default::default(),
            previous_finalize_data,
        };

        self.finalize_block_execution(
            local_ctx,
            payout_tx_value,
            subsidy,
            adjust_finalized_block,
            &mut finalize_adjustment_state.revert_state,
        )?;

        if !adjust_finalized_block {
            self.built_block_trace
                .verify_bundle_consistency(&self.building_ctx.blocklist)?;
        }

        let finalize_prep_time_ms = elapsed_ms(step_start);
        let step_start = Instant::now();

        let sim_gas_used = self.partial_block.tracer.used_gas;
        let block_number = self.building_context().block();
        let finalized_block = match self.partial_block.finalize(
            &mut self.block_state,
            &self.building_ctx,
            local_ctx,
            adjust_finalized_block,
            &mut finalize_adjustment_state,
        ) {
            Ok(finalized_block) => finalized_block,
            Err(err) => {
                if err.is_consistent_db_view_err() {
                    debug!(
                        block_number,
                        payload_id = self.building_ctx.payload_id,
                        "Can't build on this head, cancelling slot"
                    );
                    self.cancel_on_fatal_error.cancel();
                }
                return Err(BlockBuildingHelperError::FinalizeError(err));
            }
        };

        let finalize_block_time_ms = elapsed_ms(step_start);
        let finalize_time_ms = elapsed_ms(start_time);
        trace!(
            finalize_time_ms,
            finalize_prep_time_ms,
            finalize_block_time_ms,
            adjust_finalized_block,
            "Block building helper finalized block"
        );
        self.built_block_trace.update_orders_sealed_at();
        if adjust_finalized_block {
            self.built_block_trace.finalize_adjust_time = start_time.elapsed();
        } else {
            self.built_block_trace.root_hash_time = finalized_block.root_hash_time;
            self.built_block_trace.finalize_time = start_time.elapsed();
        }
        self.built_block_trace.seen_competition_bid = seen_competition_bid;
        Self::trace_finalized_block(
            &finalized_block,
            &self.builder_name,
            &self.building_ctx,
            &self.built_block_trace,
            sim_gas_used,
        );

        self.finalize_adjustment_state = Some(finalize_adjustment_state);

        let block = Block {
            builder_name: self.builder_name.clone(),
            trace: self.built_block_trace.clone(),
            sealed_block: finalized_block.sealed_block,
            txs_blobs_sidecars: finalized_block.txs_blob_sidecars,
            execution_requests: finalized_block.execution_requests,
            bid_adjustments: finalized_block.bid_adjustments,
        };
        Ok(FinalizeBlockResult { block })
    }

    fn trace_slow_order_execution(
        &self,
        order: &SimulatedOrder,
        sim_time: Duration,
        result: &Result<Result<ExecutionResult, ExecutionError>, CriticalCommitOrderError>,
    ) {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct TxInfo {
            pub hash: TxHash,
            pub signer: Address,
            pub to: Option<Address>,
        }
        impl From<&TransactionSignedEcRecoveredWithBlobs> for TxInfo {
            fn from(tx: &TransactionSignedEcRecoveredWithBlobs) -> Self {
                Self {
                    hash: tx.hash(),
                    signer: tx.signer(),
                    to: tx.to(),
                }
            }
        }
        impl TxInfo {
            fn parse_order(order: &SimulatedOrder) -> Vec<Self> {
                order
                    .order
                    .list_txs()
                    .iter()
                    .map(|(tx, _)| (*tx).into())
                    .collect::<Vec<_>>()
            }
        }
        match result {
            Ok(Ok(result)) => {
                warn!(?sim_time,builder_name=self.builder_name,id = ?order.id(),tob_sim_value = ?order.sim_value,txs = ?TxInfo::parse_order(order),
                    space_used = ?result.space_used,coinbase_profit = ?result.coinbase_profit,inplace_sim = ?result.inplace_sim, "Slow order ok execution");
            }
            Ok(Err(err)) => {
                warn!(?err,?sim_time,builder_name=self.builder_name,id = ?order.id(),tob_sim_value = ?order.sim_value,txs = ?TxInfo::parse_order(order), "Slow order failed execution.");
            }
            Err(err) => {
                warn!(?err,?sim_time,builder_name=self.builder_name,id = ?order.id(),tob_sim_value = ?order.sim_value,txs = ?TxInfo::parse_order(order), "Slow order critical execution error.");
            }
        }
    }
}

impl<
        PartialBlockExecutionTracerType: PartialBlockExecutionTracer + Clone + Send + Sync + 'static,
    > BlockBuildingHelper for BlockBuildingHelperFromProvider<PartialBlockExecutionTracerType>
{
    /// Forwards to partial_block and updates trace.
    fn commit_order(
        &mut self,
        local_ctx: &mut ThreadBlockBuildingContext,
        order: &SimulatedOrder,
        result_filter: &dyn Fn(&SimValue) -> Result<(), ExecutionError>,
    ) -> Result<Result<&ExecutionResult, ExecutionError>, CriticalCommitOrderError> {
        self.built_block_trace.add_considered_order(order);
        let start = Instant::now();
        let result = self.partial_block.commit_order(
            order,
            &self.building_ctx,
            local_ctx,
            &mut self.block_state,
            result_filter,
        );
        let sim_time = start.elapsed();
        if self
            .max_order_execution_duration_warning
            .is_some_and(|max_dur| sim_time > max_dur)
        {
            self.trace_slow_order_execution(order, sim_time, &result);
        }

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
                    self.built_block_trace.add_failed_order(order);
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

    fn true_block_value(&self) -> Result<U256, BlockBuildingHelperError> {
        Ok(self
            .partial_block
            .get_proposer_payout_tx_value(self.payout_tx_gas, &self.building_ctx)?)
    }

    fn finalize_block(
        &mut self,
        local_ctx: &mut ThreadBlockBuildingContext,
        payout_tx_value: U256,
        subsidy: I256,
        seen_competition_bid: Option<U256>,
    ) -> Result<FinalizeBlockResult, BlockBuildingHelperError> {
        self.finalize_block_impl(
            local_ctx,
            payout_tx_value,
            subsidy,
            seen_competition_bid,
            false,
        )
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

    fn builder_name(&self) -> &str {
        &self.builder_name
    }

    fn set_filtered_build_statistics(
        &mut self,
        considered_orders_statistics: OrderStatistics,
        failed_orders_statistics: OrderStatistics,
    ) {
        self.built_block_trace
            .set_filtered_build_statistics(considered_orders_statistics, failed_orders_statistics);
    }

    fn adjust_finalized_block(
        &mut self,
        local_ctx: &mut ThreadBlockBuildingContext,
        payout_tx_value: U256,
        subsidy: I256,
        seen_competition_bid: Option<U256>,
    ) -> Result<FinalizeBlockResult, BlockBuildingHelperError> {
        self.finalize_block_impl(
            local_ctx,
            payout_tx_value,
            subsidy,
            seen_competition_bid,
            true,
        )
    }
}
