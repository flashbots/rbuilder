use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{utils::format_ether, TxHash, I256, U256};

use crate::building::{
    BlockBuildingSpaceState, CriticalCommitOrderError, ExecutionError, ExecutionResult,
    PartialBlockExecutionTracer, PartialBlockForkExecutionTracer, TransactionErr, TransactionOk,
};
use rbuilder_primitives::{OrderId, TransactionSignedEcRecoveredWithBlobs};

const INDENT_SIZE: usize = 2;

trait ItemSummary: std::fmt::Debug + Send + Sync {
    fn print_summary(&self);
    /// Only valid for orders
    #[allow(unused)]
    fn total_profit_after_execution(&self) -> Option<U256>;
    #[allow(unused)]
    fn execution_end(&self) -> Duration;
}

#[derive(Debug, Clone)]
struct BaseExecutionSummary {
    /// Relative to execution_start (eg:first tx or order will be 0)
    execution_start: Duration,
    /// Time spent in the execution
    execution_time: Duration,
}

impl BaseExecutionSummary {
    #[allow(unused)]
    fn execution_end(&self) -> Duration {
        self.execution_start + self.execution_time
    }
}

/// Cheaper error, particularly if does not contain logs for OkSuccess
#[derive(Debug, Clone)]
enum SimpleTxExecutionResult {
    OkSuccess,
    OkRevert,
    OkHalt,
    CriticalCommitOrderError,
    Err,
}

impl SimpleTxExecutionResult {
    fn label(&self) -> &'static str {
        match self {
            SimpleTxExecutionResult::OkSuccess => "OK ",
            SimpleTxExecutionResult::OkRevert => "OKR",
            SimpleTxExecutionResult::OkHalt => "OKH",
            SimpleTxExecutionResult::CriticalCommitOrderError => "CRI",
            SimpleTxExecutionResult::Err => "ERR",
        }
    }
}

/// Single tx execution summary.
#[derive(Debug, Clone)]
struct TxExecutionSummary {
    base: BaseExecutionSummary,
    hash: TxHash,
    coinbase_delta: I256,
    gas_used: u64,
    result: SimpleTxExecutionResult,
}

impl TxExecutionSummary {
    fn print_summary_indented(&self, indent: usize) {
        println!(
            "{} {: <indent$}TX {:?} {} D {:>22} G {:>8} {}",
            format_duration(self.base.execution_start),
            "",
            self.hash,
            format_duration(self.base.execution_time),
            format_ether(self.coinbase_delta),
            self.gas_used,
            self.result.label()
        );
    }
}

impl ItemSummary for TxExecutionSummary {
    fn print_summary(&self) {
        self.print_summary_indented(0);
    }

    fn total_profit_after_execution(&self) -> Option<U256> {
        None
    }

    fn execution_end(&self) -> Duration {
        self.base.execution_end()
    }
}

#[derive(Debug, Clone)]
enum SimpleOrderExecutionResult {
    Ok,
    CriticalCommitOrderError,
    /// ExecutionError::OrderError
    OrderError,
    /// ExecutionError::LowerInsertedValue
    LowProfit,
}

/// Order execution summary. Aggregates children TxExecutionSummary.
#[derive(Debug, Clone)]
struct OrderExecutionSummary {
    id: OrderId,
    base: BaseExecutionSummary,
    result: SimpleOrderExecutionResult,
    order_executed_txs: Vec<TxExecutionSummary>,
    profit: Option<U256>,
    total_profit_after_execution: U256,
    sim_profit: U256,
}

impl SimpleOrderExecutionResult {
    fn label(&self) -> &'static str {
        match self {
            SimpleOrderExecutionResult::Ok => "OK ",
            SimpleOrderExecutionResult::CriticalCommitOrderError => "CRI",
            SimpleOrderExecutionResult::OrderError => "ERR",
            SimpleOrderExecutionResult::LowProfit => "LOW",
        }
    }
}

fn format_duration(duration: Duration) -> String {
    format!("{:8.2}ms", duration.as_secs_f64() * 1000.0)
}

impl ItemSummary for OrderExecutionSummary {
    fn print_summary(&self) {
        println!(
            "{} Order {:?>60} {} {}/{} ETH T {} {}",
            format_duration(self.base.execution_start),
            self.id,
            format_duration(self.base.execution_time),
            self.profit
                .map(format_ether)
                .unwrap_or("--------None--------".to_string()),
            format_ether(self.sim_profit),
            format_ether(self.total_profit_after_execution),
            self.result.label()
        );
        for tx in &self.order_executed_txs {
            tx.print_summary_indented(INDENT_SIZE);
        }
    }

    fn total_profit_after_execution(&self) -> Option<U256> {
        Some(self.total_profit_after_execution)
    }

    fn execution_end(&self) -> Duration {
        self.base.execution_end()
    }
}

/// Tracer that stores all the info and on Drop prints a report.
#[derive(Debug, Clone)]
pub struct FullPartialBlockExecutionTracer {
    /// Start of the execution (new())
    execution_start: Option<Instant>,
    creation_time: Instant,
    /// Some while inside an order
    last_order_start_time: Option<Instant>,
    last_tx_start_time: Instant,
    /// While inside an order we store tx results here and log them when the order is finished.
    order_executed_txs: Vec<TxExecutionSummary>,
    total_profit: U256,
    /// Every log item is stored here to be printed on Drop.
    log: Vec<Arc<dyn ItemSummary>>,
}

impl FullPartialBlockExecutionTracer {
    pub fn new() -> Self {
        Self {
            creation_time: Instant::now(),
            execution_start: None,
            last_order_start_time: None,
            last_tx_start_time: Instant::now(),
            order_executed_txs: Vec::new(),
            total_profit: U256::ZERO,
            log: Vec::new(),
        }
    }
}

impl PartialBlockExecutionTracer for FullPartialBlockExecutionTracer {
    fn update_commit_order_about_to_execute(
        &mut self,
        _order: &rbuilder_primitives::SimulatedOrder,
    ) {
        assert!(self.last_order_start_time.is_none());
        assert!(self.order_executed_txs.is_empty());
        self.last_order_start_time = Some(Instant::now());
        if self.execution_start.is_none() {
            self.execution_start = self.last_order_start_time;
        }
    }

    fn update_commit_order_executed(
        &mut self,
        order: &rbuilder_primitives::SimulatedOrder,
        res: &Result<Result<ExecutionResult, ExecutionError>, CriticalCommitOrderError>,
    ) {
        let base = BaseExecutionSummary {
            execution_start: self.last_order_start_time.unwrap() - self.execution_start.unwrap(),
            execution_time: self.last_order_start_time.take().unwrap().elapsed(),
        };
        let (profit, result) = match res {
            Ok(inner_res) => match inner_res {
                Ok(execution_result) => {
                    self.total_profit += execution_result.coinbase_profit;
                    (
                        Some(execution_result.coinbase_profit),
                        SimpleOrderExecutionResult::Ok,
                    )
                }
                Err(error) => {
                    let result = match error {
                        ExecutionError::OrderError(_) => SimpleOrderExecutionResult::OrderError,
                        ExecutionError::LowerInsertedValue {
                            before: _,
                            inplace: _,
                        } => SimpleOrderExecutionResult::LowProfit,
                    };
                    (None, result)
                }
            },
            Err(_) => (None, SimpleOrderExecutionResult::CriticalCommitOrderError),
        };
        let summary = OrderExecutionSummary {
            id: order.order.id(),
            base,
            order_executed_txs: std::mem::take(&mut self.order_executed_txs),
            profit,
            result,
            total_profit_after_execution: self.total_profit,
            sim_profit: order.sim_value.full_profit_info().coinbase_profit(),
        };
        self.log.push(Arc::new(summary));
    }
}

impl PartialBlockForkExecutionTracer for FullPartialBlockExecutionTracer {
    fn update_commit_tx_about_to_execute(
        &mut self,
        _tx_with_blobs: &TransactionSignedEcRecoveredWithBlobs,
        _space_state: BlockBuildingSpaceState,
    ) {
        self.last_tx_start_time = Instant::now();
        if self.execution_start.is_none() {
            self.execution_start = Some(self.last_tx_start_time);
        }
    }

    fn update_commit_tx_executed(
        &mut self,
        tx_with_blobs: &rbuilder_primitives::TransactionSignedEcRecoveredWithBlobs,
        _space_state: BlockBuildingSpaceState,
        res: &Result<Result<TransactionOk, TransactionErr>, CriticalCommitOrderError>,
    ) {
        let base = BaseExecutionSummary {
            execution_start: self.last_tx_start_time - self.execution_start.unwrap(),
            execution_time: self.last_tx_start_time.elapsed(),
        };
        let (result, coinbase_delta, gas_used) = match &res {
            Ok(Ok(tx_ok)) => match tx_ok.exec_result {
                revm::context::result::ExecutionResult::Success {
                    reason: _,
                    gas_used: _,
                    gas_refunded: _,
                    logs: _,
                    output: _,
                } => (
                    SimpleTxExecutionResult::OkSuccess,
                    tx_ok.tx_info.coinbase_profit,
                    tx_ok.tx_info.space_used.gas,
                ),
                revm::context::result::ExecutionResult::Revert {
                    gas_used: _,
                    output: _,
                } => (
                    SimpleTxExecutionResult::OkRevert,
                    tx_ok.tx_info.coinbase_profit,
                    tx_ok.tx_info.space_used.gas,
                ),
                revm::context::result::ExecutionResult::Halt {
                    reason: _,
                    gas_used: _,
                } => (
                    SimpleTxExecutionResult::OkHalt,
                    tx_ok.tx_info.coinbase_profit,
                    tx_ok.tx_info.space_used.gas,
                ),
            },
            Ok(Err(_)) => (SimpleTxExecutionResult::Err, I256::ZERO, 0),
            Err(_) => (
                SimpleTxExecutionResult::CriticalCommitOrderError,
                I256::ZERO,
                0,
            ),
        };
        let summary = TxExecutionSummary {
            base,
            hash: tx_with_blobs.hash(),
            coinbase_delta,
            gas_used,
            result,
        };
        if self.last_order_start_time.is_some() {
            self.order_executed_txs.push(summary);
        } else {
            self.log.push(Arc::new(summary));
        }
    }
}

impl Drop for FullPartialBlockExecutionTracer {
    fn drop(&mut self) {
        println!(
            "Total time: {} First order delay {}",
            format_duration(self.creation_time.elapsed()),
            format_duration(self.execution_start.unwrap() - self.creation_time)
        );
        for item in &self.log {
            item.print_summary();
        }
    }
}
