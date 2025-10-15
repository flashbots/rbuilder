use crate::{
    building::{
        builders::BuiltBlockId, BlockBuildingContext, BuiltBlockTrace, CriticalCommitOrderError,
        ExecutionError, ExecutionResult, ThreadBlockBuildingContext,
    },
    live_builder::simulation::SimulatedOrderCommand,
    provider::RootHasher,
    roothash::RootHashError,
};
use alloy_primitives::{Address, Bytes, B256, I256, U256};
use eth_sparse_mpt::utils::{HashMap, HashSet};
use rbuilder_primitives::{order_statistics::OrderStatistics, SimValue, SimulatedOrder};
use reth_primitives::SealedBlock;
use revm::database::BundleState;
use time::OffsetDateTime;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::{
    block_building_helper::{BlockBuildingHelper, BlockBuildingHelperError, FinalizeBlockResult},
    Block,
};

/// Extremely dumb object for test. Adding orders (commit_order) is not allowed.
/// Is has a predefined true_block_value and the only useful thing that generates on finalize_block is the bid value.
#[derive(Clone, Debug)]
pub struct MockBlockBuildingHelper {
    built_block_trace: BuiltBlockTrace,
    block_building_context: BlockBuildingContext,
    builder_name: String,
}

impl MockBlockBuildingHelper {
    pub fn new(true_block_value: U256) -> Self {
        let built_block_trace = BuiltBlockTrace {
            true_bid_value: true_block_value,
            ..BuiltBlockTrace::new(BuiltBlockId::ZERO)
        };
        Self {
            built_block_trace,
            block_building_context: BlockBuildingContext::dummy_for_testing(),
            builder_name: "Mock".to_string(),
        }
    }

    pub fn with_builder_name(self, builder_name: String) -> Self {
        Self {
            builder_name,
            ..self
        }
    }

    pub fn built_block_trace_mut_ref(&mut self) -> &mut BuiltBlockTrace {
        &mut self.built_block_trace
    }
}

impl BlockBuildingHelper for MockBlockBuildingHelper {
    fn box_clone(&self) -> Box<dyn BlockBuildingHelper> {
        Box::new(self.clone())
    }

    fn commit_order(
        &mut self,
        _local_ctx: &mut ThreadBlockBuildingContext,
        _order: &SimulatedOrder,
        _result_filter: &dyn Fn(&SimValue) -> Result<(), ExecutionError>,
    ) -> Result<Result<&ExecutionResult, ExecutionError>, CriticalCommitOrderError> {
        unimplemented!()
    }

    fn set_trace_fill_time(&mut self, time: std::time::Duration) {
        self.built_block_trace.fill_time = time;
    }

    fn set_trace_orders_closed_at(&mut self, orders_closed_at: OffsetDateTime) {
        self.built_block_trace.orders_closed_at = orders_closed_at;
    }

    fn true_block_value(&self) -> Result<U256, BlockBuildingHelperError> {
        Ok(self.built_block_trace.true_bid_value)
    }

    fn finalize_block(
        &mut self,
        _local_ctx: &mut ThreadBlockBuildingContext,
        payout_tx_value: U256,
        subsidy: I256,
        seen_competition_bid: Option<U256>,
    ) -> Result<FinalizeBlockResult, BlockBuildingHelperError> {
        self.built_block_trace.update_orders_sealed_at();
        self.built_block_trace.seen_competition_bid = seen_competition_bid;
        self.built_block_trace.bid_value = payout_tx_value;
        self.built_block_trace.subsidy = subsidy;
        let block = Block {
            builder_name: "BlockBuildingHelper".to_string(),
            trace: self.built_block_trace.clone(),
            sealed_block: SealedBlock::default(),
            txs_blobs_sidecars: Vec::new(),
            execution_requests: Default::default(),
            bid_adjustments: Default::default(),
        };

        Ok(FinalizeBlockResult { block })
    }

    fn built_block_trace(&self) -> &BuiltBlockTrace {
        &self.built_block_trace
    }

    fn building_context(&self) -> &BlockBuildingContext {
        &self.block_building_context
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
        _local_ctx: &mut ThreadBlockBuildingContext,
        _payout_tx_value: U256,
        _subsidy: I256,
        _seen_competition_bid: Option<U256>,
    ) -> Result<FinalizeBlockResult, BlockBuildingHelperError> {
        unimplemented!()
    }
}

#[derive(Debug)]
pub struct MockRootHasher {}

impl RootHasher for MockRootHasher {
    fn run_prefetcher(
        &self,
        _simulated_orders: broadcast::Receiver<SimulatedOrderCommand>,
        _cancel: CancellationToken,
    ) {
    }

    fn account_proofs(
        &self,
        _outcome: &BundleState,
        _addresses: &HashSet<Address>,
        _local_ctx: &mut ThreadBlockBuildingContext,
    ) -> Result<HashMap<Address, Vec<Bytes>>, RootHashError> {
        Ok(Default::default())
    }

    fn state_root(
        &self,
        _outcome: &BundleState,
        _incremental_change: &[Address],
        _local_ctx: &mut ThreadBlockBuildingContext,
    ) -> Result<B256, RootHashError> {
        Ok(B256::default())
    }
}
