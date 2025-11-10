/// Tracing metrics are used to get a fine grained look at the path of the order through the builder.
/// To start collecting this metric mark_building_started must be called at the start of each slot.
use crate::live_builder::order_input::ReplaceableOrderPoolCommand;
use ahash::RandomState;
use dashmap::{DashMap, DashSet};
use lazy_static::lazy_static;
use rbuilder_primitives::{Order, OrderId};
use std::sync::{Arc, RwLock};
use time::OffsetDateTime;

use super::{
    sim_status, BLOCK_METRICS_TIMESTAMP_LOWER_DELTA, BLOCK_METRICS_TIMESTAMP_UPPER_DELTA,
    ORDERPOOL_ORDERS_RECEIVED, ORDER_RECEIVED_TO_SIM_END_TIME,
    ORDER_SIM_END_TO_FIRST_BUILD_STARTED_MIN_TIME, ORDER_SIM_END_TO_FIRST_BUILD_STARTED_TIME,
};

type Timestamp = u64; // timestamp in microseconds
type BuilderId = u64; // integer id to minimize string cloning

#[derive(Debug, Default)]
struct TracingMetricsData {
    last_slot_critical_period: Arc<RwLock<(Timestamp, Timestamp)>>,

    builder_by_name: Arc<DashMap<String, u64, RandomState>>,

    // All fields below must be cleaned once per slot in `mark_building_started`
    orders_received: Arc<DashMap<OrderId, Timestamp, RandomState>>,
    orders_with_pending_nonces: Arc<DashSet<OrderId, RandomState>>,
    orders_simulation_end: Arc<DashMap<OrderId, Timestamp, RandomState>>,

    orders_not_ready_for_immediate_inclusion: Arc<DashSet<OrderId, RandomState>>,
    orders_first_insertion_block_seal_start_by_builder:
        Arc<DashMap<(OrderId, BuilderId), Timestamp, RandomState>>,
    orders_first_insertion_block_seal_start:
        Arc<DashMap<OrderId, (Timestamp, BuilderId), RandomState>>,
}

lazy_static! {
    static ref METRICS_TRACING_REGISTRY: TracingMetricsData = TracingMetricsData::default();
}

// this should be called on each of the tracing metric invocation to prevent memory leak when
// mark_building_started is not called
fn should_record_tracing_metric(timestamp: &OffsetDateTime) -> bool {
    let (start, end) = *METRICS_TRACING_REGISTRY
        .last_slot_critical_period
        .read()
        .unwrap();
    if start == 0 || end == 0 {
        return false;
    }
    let time = offset_datetime_to_timestamp_us(timestamp);

    let too_early = time < start;
    let too_late = time > end;
    !too_early && !too_late
}

fn get_builder_id(builder_name: &str) -> BuilderId {
    if let Some(id) = METRICS_TRACING_REGISTRY.builder_by_name.get(builder_name) {
        return *id;
    }
    let id: u64 = rand::random();
    METRICS_TRACING_REGISTRY
        .builder_by_name
        .insert(builder_name.to_string(), id);
    id
}

/// mark_building_started should be called on each slot start to mark building starting time and to clean accumulated data.
/// If its not called tracing data is not collected.
pub fn mark_building_started(block_timestamp: OffsetDateTime) {
    let reg = &METRICS_TRACING_REGISTRY;
    {
        let start = (block_timestamp - BLOCK_METRICS_TIMESTAMP_LOWER_DELTA).unix_timestamp_nanos()
            as u64
            / 1000;
        let end = (block_timestamp + BLOCK_METRICS_TIMESTAMP_UPPER_DELTA).unix_timestamp_nanos()
            as u64
            / 1000;
        let mut last_slot_period = reg.last_slot_critical_period.write().unwrap();
        *last_slot_period = (start, end);
    }

    reg.orders_received.clear();
    reg.orders_with_pending_nonces.clear();
    reg.orders_simulation_end.clear();
    reg.orders_not_ready_for_immediate_inclusion.clear();
    reg.orders_first_insertion_block_seal_start_by_builder
        .clear();
    reg.orders_first_insertion_block_seal_start.clear();
}

/// This should be called when ordrepool command appears in the builder. It can be a new order or order replacement.
pub fn mark_command_received(command: &ReplaceableOrderPoolCommand, received_at: OffsetDateTime) {
    let kind = match command {
        ReplaceableOrderPoolCommand::Order(order) => {
            mark_order_received(order.id(), received_at);
            match order {
                Order::Bundle(_) => "bundle",
                Order::Tx(_) => "tx",
                Order::ShareBundle(_) => "sbundle",
            }
        }
        ReplaceableOrderPoolCommand::CancelShareBundle(_)
        | ReplaceableOrderPoolCommand::CancelBundle(_) => "cancel",
    };
    ORDERPOOL_ORDERS_RECEIVED.with_label_values(&[kind]).inc();
}

fn mark_order_received(id: OrderId, received_at: OffsetDateTime) {
    if !should_record_tracing_metric(&received_at) {
        return;
    }

    if METRICS_TRACING_REGISTRY.orders_received.contains_key(&id) {
        return;
    }
    let timestamp = offset_datetime_to_timestamp_us(&received_at);
    METRICS_TRACING_REGISTRY
        .orders_received
        .insert(id, timestamp);
}

/// mark_order_pending_nonce should be called when order that was received can't be simulated immediately because of the nonce.
pub fn mark_order_pending_nonce(id: OrderId) {
    let now = OffsetDateTime::now_utc();
    if !should_record_tracing_metric(&now) {
        return;
    }

    METRICS_TRACING_REGISTRY
        .orders_with_pending_nonces
        .insert(id);
}

/// mark_order_simulation_end should be called when order top of block simulation ends.
pub fn mark_order_simulation_end(id: OrderId, success: bool) {
    let now = OffsetDateTime::now_utc();
    if !should_record_tracing_metric(&now) {
        return;
    }

    let received_at = if let Some(ts) = METRICS_TRACING_REGISTRY.orders_received.get(&id) {
        *ts
    } else {
        return;
    };

    if METRICS_TRACING_REGISTRY
        .orders_simulation_end
        .contains_key(&id)
    {
        return;
    }

    // we con't record metrics for ordrers that were stuck due to nonce
    if METRICS_TRACING_REGISTRY
        .orders_with_pending_nonces
        .contains(&id)
    {
        return;
    }

    let now = offset_datetime_to_timestamp_us(&now);
    METRICS_TRACING_REGISTRY
        .orders_simulation_end
        .insert(id, now);

    let received_to_sim_end_time_ms = if received_at < now {
        let time_us = (now - received_at) as f64;
        time_us / 1000.0
    } else {
        return;
    };

    ORDER_RECEIVED_TO_SIM_END_TIME
        .with_label_values(&[sim_status(success)])
        .observe(received_to_sim_end_time_ms);
}

/// mark_order_not_ready_for_immediate_inclusion should be called if order can't be included immediatly.
/// For example, if it was invalidated by nonce by other order inclusion.
pub fn mark_order_not_ready_for_immediate_inclusion(order_id: &OrderId) {
    let now = OffsetDateTime::now_utc();
    if !should_record_tracing_metric(&now) {
        return;
    }

    if METRICS_TRACING_REGISTRY
        .orders_not_ready_for_immediate_inclusion
        .contains(order_id)
    {
        return;
    };
    METRICS_TRACING_REGISTRY
        .orders_not_ready_for_immediate_inclusion
        .insert(*order_id);
}

/// mark_builder_considers_order should be called when builder considers order for inclusion
/// order_closed_at is a time at which builder stopped considering new orders for the current run
pub fn mark_builder_considers_order(
    order_id: OrderId,
    order_closed_at: &OffsetDateTime,
    builder_name: &str,
) {
    if !should_record_tracing_metric(order_closed_at) {
        return;
    }

    let builder_id = get_builder_id(builder_name);
    if METRICS_TRACING_REGISTRY
        .orders_first_insertion_block_seal_start_by_builder
        .contains_key(&(order_id, builder_id))
    {
        return;
    }

    let order_sim_end_time = METRICS_TRACING_REGISTRY
        .orders_simulation_end
        .get(&order_id)
        .map(|r| *r)
        .unwrap_or_default();
    let ready_for_immediate_inclusion = METRICS_TRACING_REGISTRY
        .orders_not_ready_for_immediate_inclusion
        .contains(&order_id);

    let timestamp = offset_datetime_to_timestamp_us(order_closed_at);
    let min_time_set = if !METRICS_TRACING_REGISTRY
        .orders_first_insertion_block_seal_start
        .contains_key(&order_id)
    {
        METRICS_TRACING_REGISTRY
            .orders_first_insertion_block_seal_start
            .insert(order_id, (builder_id, timestamp));
        true
    } else {
        false
    };

    METRICS_TRACING_REGISTRY
        .orders_first_insertion_block_seal_start_by_builder
        .insert((order_id, builder_id), timestamp);

    if order_sim_end_time == 0 || order_sim_end_time > timestamp || ready_for_immediate_inclusion {
        return;
    }

    ORDER_SIM_END_TO_FIRST_BUILD_STARTED_TIME
        .with_label_values(&[builder_name])
        .observe((timestamp - order_sim_end_time) as f64 / 1000.0);
    if min_time_set {
        ORDER_SIM_END_TO_FIRST_BUILD_STARTED_MIN_TIME
            .with_label_values(&[builder_name])
            .observe((timestamp - order_sim_end_time) as f64 / 1000.0);
    }
}

fn offset_datetime_to_timestamp_us(dt: &OffsetDateTime) -> Timestamp {
    (dt.unix_timestamp_nanos() / 1_000)
        .try_into()
        .unwrap_or_default()
}
