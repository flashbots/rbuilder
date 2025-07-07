use std::time::Instant;

use crate::building::{PartialBlockExecutionTracer, PartialBlockForkExecutionTracer};
#[derive(Debug, Clone)]
pub struct FullPartialBlockExecutionTracer {
    last_order_start_time: Instant,
    last_tx_start_time: Instant,
}

impl FullPartialBlockExecutionTracer {
    pub fn new() -> Self {
        Self {
            last_order_start_time: Instant::now(),
            last_tx_start_time: Instant::now(),
        }
    }
}

impl PartialBlockExecutionTracer for FullPartialBlockExecutionTracer {
    fn update_commit_order_about_to_execute(&mut self, _order: &crate::primitives::SimulatedOrder) {
        self.last_order_start_time = Instant::now();
    }

    fn update_commit_order_executed(
        &mut self,
        order: &crate::primitives::SimulatedOrder,
        res: &Result<
            Result<crate::building::ExecutionResult, crate::building::ExecutionError>,
            crate::building::CriticalCommitOrderError,
        >,
    ) {
        let delta = self.last_order_start_time.elapsed();
        let result = if let Ok(Ok(_)) = res {
            "OK    "
        } else {
            "ERROR "
        };

        println!(
            "Order {:?} executed in {:?} {result}",
            order.order.id(),
            delta
        );
    }
}

impl PartialBlockForkExecutionTracer for FullPartialBlockExecutionTracer {
    fn update_commit_tx_about_to_execute(
        &mut self,
        _tx_with_blobs: &crate::primitives::TransactionSignedEcRecoveredWithBlobs,
        _cumulative_gas_used: u64,
        _gas_reserved: u64,
        _cumulative_blob_gas_used: u64,
    ) {
        self.last_tx_start_time = Instant::now();
    }

    fn update_commit_tx_executed(
        &mut self,
        tx_with_blobs: &crate::primitives::TransactionSignedEcRecoveredWithBlobs,
        _cumulative_gas_used: u64,
        _gas_reserved: u64,
        _cumulative_blob_gas_used: u64,
        res: &Result<
            Result<crate::building::TransactionOk, crate::building::TransactionErr>,
            crate::building::CriticalCommitOrderError,
        >,
    ) {
        let delta = self.last_order_start_time.elapsed();
        let result = if let Ok(Ok(_)) = res {
            "OK    "
        } else {
            "ERROR "
        };

        println!(
            "   TX {:?} executed in {:?} {result}",
            tx_with_blobs.hash(),
            delta
        );
    }
}
