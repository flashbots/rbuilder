use std::time::Duration;

use alloy_primitives::{TxHash, U256};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use rbuilder_primitives::{
    BundleReplacementData, OrderId, OrderReplacementKey, ShareBundleReplacementKey,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct EventWithTimestamp<EventType> {
    pub event: EventType,
    pub timestamp: OffsetDateTime,
}

impl<EventType> EventWithTimestamp<EventType> {
    pub fn new(event: EventType) -> Self {
        Self {
            event,
            timestamp: OffsetDateTime::now_utc(),
        }
    }
}

/// If this grows a lot we could consider storing the Arc<SimulatedOrder> instead and do the extra work when we make the report.
#[derive(Debug, Serialize, Deserialize)]
pub struct SimulatedOrderData {
    pub order_id: OrderId,
    pub replacement_key_and_sequence_number: Option<(OrderReplacementKey, u64)>,
    pub simulation_time: Duration,
    pub full_profit: U256,
    pub non_mempool_profit: U256,
    pub gas_used: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum SimulationEvent {
    SimulatedOrder(SimulatedOrderData),
    CancellationSent(OrderId),
}

pub type SimulationEventWithTimestamp = EventWithTimestamp<SimulationEvent>;

/// Since Order is expensive to clone we take what we need.
#[derive(Debug, Serialize, Deserialize)]
pub struct InsertOrderData {
    pub order_id: OrderId,
    pub replacement_key_and_sequence_number: Option<(OrderReplacementKey, u64)>,
    pub tx_hashes: Vec<TxHash>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ReplaceableOrderEvent {
    InsertOrder(InsertOrderData),
    RemoveBundle(BundleReplacementData),
    RemoveSBundle(ShareBundleReplacementKey),
}

pub type ReplaceableOrderEventWithTimestamp = EventWithTimestamp<ReplaceableOrderEvent>;
