use crate::building::evm_inspector::UsedStateTrace;

/// Trait to trace ANY use of an EVM instance for metrics
pub trait SimulationTracer {
    /// En EVM instance executed a tx consuming gas.
    /// This includes reverting transactions.
    fn gas_used(&mut self, _gas: u64) {}

    fn get_used_state_tracer(&mut self) -> Option<&mut UsedStateTrace> {
        None
    }
}

impl SimulationTracer for () {}

#[derive(Debug, Default, Clone)]
pub struct GasUsedSimulationTracer {
    pub used_gas: u64,
}

impl SimulationTracer for GasUsedSimulationTracer {
    fn gas_used(&mut self, gas: u64) {
        self.used_gas += gas;
    }
}

/// Tracer that accumulates gas and used state.
#[derive(Debug)]
pub struct AccumulatorSimulationTracer {
    pub used_gas: u64,
    pub used_state_trace: UsedStateTrace,
}

impl AccumulatorSimulationTracer {
    pub fn new() -> Self {
        Self {
            used_gas: 0,
            used_state_trace: UsedStateTrace::default(),
        }
    }
}

impl Default for AccumulatorSimulationTracer {
    fn default() -> Self {
        Self::new()
    }
}

impl SimulationTracer for AccumulatorSimulationTracer {
    fn gas_used(&mut self, gas: u64) {
        self.used_gas += gas;
    }

    fn get_used_state_tracer(&mut self) -> Option<&mut UsedStateTrace> {
        Some(&mut self.used_state_trace)
    }
}
