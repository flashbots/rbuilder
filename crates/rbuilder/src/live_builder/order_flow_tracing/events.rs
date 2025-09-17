use std::{sync::Arc, time::Duration};

use alloy_primitives::TxHash;
use time::OffsetDateTime;

use crate::primitives::{
    BundleReplacementData, OrderId, OrderReplacementKey, ShareBundleReplacementKey, SimulatedOrder,
};

#[derive(Debug)]
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

#[derive(Debug)]
pub struct SimulatedOrderData {
    pub sim_order: Arc<SimulatedOrder>,
    pub simulation_time: Duration,
}

#[derive(Debug)]
pub enum SimulationEvent {
    SimulatedOrder(SimulatedOrderData),
    CancellationSent(OrderId),
}

pub type SimulationEventWithTimestamp = EventWithTimestamp<SimulationEvent>;

/// Since Order is expensive to clone we take what we need.
#[derive(Debug)]
pub struct InsertOrderData {
    pub order_id: OrderId,
    pub replacement_key_and_sequence_number: Option<(OrderReplacementKey, u64)>,
    pub tx_hashes: Vec<TxHash>,
}

#[derive(Debug)]
pub enum ReplaceableOrderEvent {
    InsertOrder(InsertOrderData),
    RemoveBundle(BundleReplacementData),
    RemoveSBundle(ShareBundleReplacementKey),
}

pub type ReplaceableOrderEventWithTimestamp = EventWithTimestamp<ReplaceableOrderEvent>;
