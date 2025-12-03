use rbuilder_primitives::evm_inspector::UsedStateTrace;

/// Trait to trace ANY use of an EVM instance for metrics
pub trait SimulationTracer {
    /// En EVM instance executed a tx consuming gas.
    /// This includes reverting transactions.
    fn add_gas_used(&mut self, _gas: u64) {}

    /// If tracer returns true tx_commit will call add_used_state_trace with the given transaction trace.
    fn should_collect_used_state_trace(&self) -> bool {
        false
    }

    fn add_used_state_trace(&mut self, _trace: &UsedStateTrace) {}

    fn get_used_state_tracer(&self) -> Option<&UsedStateTrace> {
        None
    }
}

impl SimulationTracer for () {}

#[derive(Debug, Default, Clone)]
pub struct GasUsedSimulationTracer {
    pub used_gas: u64,
}

impl SimulationTracer for GasUsedSimulationTracer {
    fn add_gas_used(&mut self, gas: u64) {
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
    fn add_gas_used(&mut self, gas: u64) {
        self.used_gas += gas;
    }

    fn should_collect_used_state_trace(&self) -> bool {
        true
    }

    fn add_used_state_trace(&mut self, trace: &UsedStateTrace) {
        self.used_state_trace.append_trace(trace);
    }

    fn get_used_state_tracer(&self) -> Option<&UsedStateTrace> {
        Some(&self.used_state_trace)
    }
}
