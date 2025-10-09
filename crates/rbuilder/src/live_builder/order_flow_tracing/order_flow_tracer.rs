use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{
    building::sim::SimulatedResult,
    live_builder::{
        block_output::bidding_service_interface::SlotBlockId,
        order_flow_tracing::events::{
            InsertOrderData, ReplaceableOrderEvent, ReplaceableOrderEventWithTimestamp,
            SimulatedOrderData, SimulationEvent, SimulationEventWithTimestamp,
        },
        order_input::replaceable_order_sink::ReplaceableOrderSink,
        simulation::simulation_job_tracer::SimulationJobTracer,
    },
};
use rbuilder_primitives::{BundleReplacementData, Order, ShareBundleReplacementKey};

/// Struct that stores all the input and simulation orderflow to later dump it.
#[derive(Debug)]
pub struct OrderFlowTracer {
    id: SlotBlockId,
    sim_events: Mutex<Vec<SimulationEventWithTimestamp>>,
    order_input_events: Mutex<Vec<ReplaceableOrderEventWithTimestamp>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OrderFlowTracerReport {
    pub sim_events: Vec<SimulationEventWithTimestamp>,
    pub order_input_events: Vec<ReplaceableOrderEventWithTimestamp>,
}

impl OrderFlowTracer {
    /// Takes the next ReplaceableOrderSink on the chain and returns the one that will be used to forward the events.
    /// Also returns the OrderFlowTracer itself.
    pub fn new(
        id: SlotBlockId,
        sink: Box<dyn ReplaceableOrderSink>,
    ) -> (Arc<Self>, Box<dyn ReplaceableOrderSink>) {
        let tracer = Arc::new(Self {
            id,
            sim_events: Mutex::new(Vec::new()),
            order_input_events: Mutex::new(Vec::new()),
        });
        (
            tracer.clone(),
            Box::new(ReplaceableOrderSniffer::new(tracer, sink)),
        )
    }

    pub fn id(&self) -> SlotBlockId {
        self.id.clone()
    }

    fn insert_order(&self, order: &Order) {
        let event = ReplaceableOrderEventWithTimestamp::new(ReplaceableOrderEvent::InsertOrder(
            InsertOrderData {
                order_id: order.id(),
                replacement_key_and_sequence_number: order.replacement_key_and_sequence_number(),
                tx_hashes: order.list_txs().iter().map(|(tx, _)| tx.hash()).collect(),
            },
        ));
        self.order_input_events.lock().push(event);
    }
    fn remove_bundle(&self, replacement_data: &BundleReplacementData) {
        let event = ReplaceableOrderEventWithTimestamp::new(ReplaceableOrderEvent::RemoveBundle(
            replacement_data.clone(),
        ));
        self.order_input_events.lock().push(event);
    }
    fn remove_sbundle(&self, key: &ShareBundleReplacementKey) {
        let event =
            ReplaceableOrderEventWithTimestamp::new(ReplaceableOrderEvent::RemoveSBundle(*key));
        self.order_input_events.lock().push(event);
    }

    pub fn into_report(self) -> OrderFlowTracerReport {
        OrderFlowTracerReport {
            sim_events: self.sim_events.into_inner(),
            order_input_events: self.order_input_events.into_inner(),
        }
    }
}

impl SimulationJobTracer for OrderFlowTracer {
    fn update_simulation_sent(&self, sim_result: &SimulatedResult) {
        let event = SimulationEvent::SimulatedOrder(SimulatedOrderData {
            simulation_time: sim_result.simulation_time,
            order_id: sim_result.simulated_order.order.id(),
            replacement_key_and_sequence_number: sim_result
                .simulated_order
                .order
                .replacement_key_and_sequence_number(),
            full_profit: sim_result
                .simulated_order
                .sim_value
                .full_profit_info()
                .coinbase_profit(),
            non_mempool_profit: sim_result
                .simulated_order
                .sim_value
                .non_mempool_profit_info()
                .coinbase_profit(),
            gas_used: sim_result.simulated_order.sim_value.gas_used(),
        });
        self.sim_events
            .lock()
            .push(SimulationEventWithTimestamp::new(event));
    }

    fn update_cancellation_sent(&self, order_id: &rbuilder_primitives::OrderId) {
        let event = SimulationEvent::CancellationSent(*order_id);
        self.sim_events
            .lock()
            .push(SimulationEventWithTimestamp::new(event));
    }
}

/// Sniffs the orderflow and forwards it to the OrderFlowTracer.
/// Mainly exists to adapt Box to Arc.
#[derive(Debug)]
struct ReplaceableOrderSniffer {
    tracer: Arc<OrderFlowTracer>,
    sink: Box<dyn ReplaceableOrderSink>,
}

impl ReplaceableOrderSniffer {
    pub fn new(tracer: Arc<OrderFlowTracer>, sink: Box<dyn ReplaceableOrderSink>) -> Self {
        Self { tracer, sink }
    }
}

impl ReplaceableOrderSink for ReplaceableOrderSniffer {
    fn insert_order(&mut self, order: Order) -> bool {
        self.tracer.insert_order(&order);
        self.sink.insert_order(order)
    }

    fn remove_bundle(&mut self, replacement_data: BundleReplacementData) -> bool {
        self.tracer.remove_bundle(&replacement_data);
        self.sink.remove_bundle(replacement_data)
    }

    fn remove_sbundle(&mut self, key: ShareBundleReplacementKey) -> bool {
        self.tracer.remove_sbundle(&key);
        self.sink.remove_sbundle(key)
    }

    fn is_alive(&self) -> bool {
        self.sink.is_alive()
    }
}
