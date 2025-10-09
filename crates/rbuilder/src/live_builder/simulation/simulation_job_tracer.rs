use crate::building::sim::SimulatedResult;
use rbuilder_primitives::OrderId;

/// Trait to trace the output of the simulation stage.
pub trait SimulationJobTracer: Send + Sync {
    /// A SimulatedOrder was sent downstream.
    /// This is called AFTER the successful send.
    fn update_simulation_sent(&self, sim_result: &SimulatedResult);
    /// A cancellation was forwarded downstream.
    /// This is called AFTER the successful send.
    fn update_cancellation_sent(&self, order_id: &OrderId);
}

pub struct NullSimulationJobTracer;
impl SimulationJobTracer for NullSimulationJobTracer {
    fn update_simulation_sent(&self, _sim_result: &SimulatedResult) {}
    fn update_cancellation_sent(&self, _order_id: &OrderId) {}
}
