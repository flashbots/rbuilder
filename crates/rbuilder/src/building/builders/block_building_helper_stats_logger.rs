use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{utils::format_ether, I256, U256};

use crate::{
    building::{builders::block_building_helper::BlockBuildingHelper, ThreadBlockBuildingContext},
    live_builder::order_input::mempool_txs_detector::MempoolTxsDetector,
};
use rbuilder_primitives::{order_statistics::OrderStatistics, OrderId, SimulatedOrder};

use super::block_building_helper::{BlockBuildingHelperError, FinalizeBlockResult};

/// Wraps a BlockBuildingHelper and stores info about every commit_order as lightweight as possible.
pub struct BlockBuildingHelperStatsLogger<'a> {
    block_building_helper: &'a mut dyn BlockBuildingHelper,
    pub logs: Vec<BlockBuildingHelperCommitLog>,
}

pub struct ExecutionResult {
    coinbase_profit: U256,
    gas_used: u64,
    landed_tx_count: usize,
}

impl ExecutionResult {
    pub fn coinbase_profit(&self) -> U256 {
        self.coinbase_profit
    }

    pub fn gas_used(&self) -> u64 {
        self.gas_used
    }

    pub fn landed_tx_count(&self) -> usize {
        self.landed_tx_count
    }
}

pub struct BlockBuildingHelperCommitLog {
    order_id: OrderId,
    time_spent: Duration,
    /// Some <-> Executed ok.
    execution_result: Option<ExecutionResult>,
}

impl BlockBuildingHelperCommitLog {
    pub fn new(
        order_id: OrderId,
        result: &Result<
            Result<&crate::building::ExecutionResult, crate::building::ExecutionError>,
            crate::building::CriticalCommitOrderError,
        >,
        time_spent: Duration,
    ) -> Self {
        let execution_result = if let Ok(Ok(exec_ok)) = result {
            Some(ExecutionResult {
                landed_tx_count: exec_ok.tx_infos.len(),
                coinbase_profit: exec_ok.coinbase_profit,
                gas_used: exec_ok.space_used.gas,
            })
        } else {
            None
        };
        Self {
            order_id,
            time_spent,
            execution_result,
        }
    }

    pub fn order_id(&self) -> &OrderId {
        &self.order_id
    }

    pub fn time_spent(&self) -> Duration {
        self.time_spent
    }

    pub fn execution_result(&self) -> &Option<ExecutionResult> {
        &self.execution_result
    }
}

impl<'a> BlockBuildingHelperStatsLogger<'a> {
    pub fn new(block_building_helper: &'a mut dyn BlockBuildingHelper) -> Self {
        Self {
            block_building_helper,
            logs: Default::default(),
        }
    }

    pub fn print(&self, orders: Vec<Arc<SimulatedOrder>>) {
        let mut order_id_to_order = HashMap::new();
        let mempool_txs_detector = MempoolTxsDetector::new();
        for sim_order in &orders {
            order_id_to_order.insert(sim_order.id(), sim_order.clone());
            mempool_txs_detector.add_tx(&sim_order.order);
        }

        println!(
            "id,time,accum time,accum profit,tx len,mempool tx len,landed tx,profit,gas,tob profit,price"
        );
        let mut accumulated_duration = Duration::ZERO;
        let accumulated_duration_cap = Duration::from_millis(200);
        let mut accumulated_profit = U256::ZERO;

        for log in &self.logs {
            let prev_accumulated_duration = accumulated_duration;
            accumulated_duration += log.time_spent;
            accumulated_profit += log
                .execution_result
                .as_ref()
                .map_or(U256::ZERO, |e| e.coinbase_profit());
            let sim_order = order_id_to_order.get(&log.order_id).unwrap();
            let non_mempool_tx_count = sim_order
                .order
                .list_txs()
                .iter()
                .map(|(t, _)| t)
                .filter(|t| mempool_txs_detector.is_mempool(t))
                .count();
            println!(
                "{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?}",
                log.order_id,
                log.time_spent.as_nanos(),
                accumulated_duration.as_millis(),
                format_ether(accumulated_profit),
                sim_order.order.list_txs_len(),
                non_mempool_tx_count,
                log.execution_result
                    .as_ref()
                    .map_or(0, |e| e.landed_tx_count),
                format_ether(
                    log.execution_result
                        .as_ref()
                        .map_or(U256::ZERO, |e| e.coinbase_profit)
                ),
                log.execution_result.as_ref().map_or(0, |e| e.gas_used),
                format_ether(sim_order.sim_value.full_profit_info().coinbase_profit()),
                format_ether(sim_order.sim_value.full_profit_info().mev_gas_price()),
            );
            /*            for (tx, _) in sim_order.order.list_txs() {
                print!("{:?},", tx.hash());
            }*/

            if prev_accumulated_duration < accumulated_duration_cap
                && accumulated_duration >= accumulated_duration_cap
            {
                println!("========================================");
            }
        }
    }
}

impl BlockBuildingHelper for BlockBuildingHelperStatsLogger<'_> {
    /// logging is not cloned
    fn box_clone(&self) -> Box<dyn BlockBuildingHelper> {
        self.block_building_helper.box_clone()
    }

    fn commit_order(
        &mut self,
        local_ctx: &mut ThreadBlockBuildingContext,
        order: &rbuilder_primitives::SimulatedOrder,
        result_filter: &dyn Fn(
            &rbuilder_primitives::SimValue,
        ) -> Result<(), crate::building::ExecutionError>,
    ) -> Result<
        Result<&crate::building::ExecutionResult, crate::building::ExecutionError>,
        crate::building::CriticalCommitOrderError,
    > {
        println!();
        println!("STARTED BUNDLE {:?}", order.id());
        let commit_start = Instant::now();
        let res = self
            .block_building_helper
            .commit_order(local_ctx, order, result_filter);

        self.logs.push(BlockBuildingHelperCommitLog::new(
            order.id(),
            &res,
            commit_start.elapsed(),
        ));
        res
    }

    fn set_trace_fill_time(&mut self, time: std::time::Duration) {
        self.block_building_helper.set_trace_fill_time(time);
    }

    fn set_trace_orders_closed_at(&mut self, orders_closed_at: time::OffsetDateTime) {
        self.block_building_helper
            .set_trace_orders_closed_at(orders_closed_at);
    }

    fn true_block_value(
        &self,
    ) -> Result<alloy_primitives::U256, super::block_building_helper::BlockBuildingHelperError>
    {
        self.block_building_helper.true_block_value()
    }

    fn finalize_block(
        &mut self,
        _local_ctx: &mut crate::building::ThreadBlockBuildingContext,
        _payout_tx_value: alloy_primitives::U256,
        _subsidy: alloy_primitives::I256,
        _seen_competition_bid: Option<alloy_primitives::U256>,
    ) -> Result<FinalizeBlockResult, BlockBuildingHelperError> {
        panic!("finalize_block not implemented. This is only for testing.");
    }

    fn built_block_trace(&self) -> &crate::building::BuiltBlockTrace {
        self.block_building_helper.built_block_trace()
    }

    fn building_context(&self) -> &crate::building::BlockBuildingContext {
        self.block_building_helper.building_context()
    }

    fn builder_name(&self) -> &str {
        self.block_building_helper.builder_name()
    }

    fn set_filtered_build_statistics(
        &mut self,
        considered_orders_statistics: OrderStatistics,
        failed_orders_statistics: OrderStatistics,
    ) {
        self.block_building_helper
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
