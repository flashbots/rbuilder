use crate::{
    live_builder::order_input::ReplaceableOrderPoolCommand,
    primitives::{Order, OrderId},
};
use ahash::{HashMap, HashSet};
use lazy_static::lazy_static;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tracing::warn;

use super::{
    sim_status, BLOCK_METRICS_TIMESTAMP_LOWER_DELTA, BLOCK_METRICS_TIMESTAMP_UPPER_DELTA,
    ORDERPOOL_ORDERS_RECEIVED, ORDER_RECEIVED_TO_SIM_END_TIME,
};

type Timestamp = u64; // timestamp in microseconds

#[derive(Debug, Default)]
struct TracingMetricsData {
    last_slot_critical_period_start: Timestamp,
    last_slot_critical_period_end: Timestamp,

    // All fields below must be cleaned once per slot in `mark_building_started`
    orders_received: HashMap<OrderId, Timestamp>,
    orders_simulation_end: HashMap<OrderId, Timestamp>,

    orders_with_pending_nonces: HashSet<OrderId>,
}

lazy_static! {
    static ref METRICS_TRACING_REGISTRY: Arc<Mutex<TracingMetricsData>> =
        Arc::new(Mutex::new(TracingMetricsData::default()));
}

const LOCK_MAX_ACCEPTABLE_WAIT_DURATION: Duration = Duration::from_micros(10);

fn lock_registry() -> MutexGuard<'static, TracingMetricsData> {
    let start = Instant::now();
    let guard = METRICS_TRACING_REGISTRY.lock().unwrap();
    let wait_time = start.elapsed();
    if wait_time > LOCK_MAX_ACCEPTABLE_WAIT_DURATION {
        warn!(
            wait_time_us = wait_time.as_micros(),
            "Contentious lock in tracing_metrics"
        );
    }
    guard
}

// This should be called on each slot start to mark building stating time and to clean accumulated data.
// If its not called tracing data is not collected.
pub fn mark_building_started(block_timestamp: OffsetDateTime) {
    let mut reg = lock_registry();
    reg.last_slot_critical_period_start = (block_timestamp - BLOCK_METRICS_TIMESTAMP_LOWER_DELTA)
        .unix_timestamp_nanos() as u64
        / 1000;
    reg.last_slot_critical_period_end = (block_timestamp + BLOCK_METRICS_TIMESTAMP_UPPER_DELTA)
        .unix_timestamp_nanos() as u64
        / 1000;

    reg.orders_received.clear();
    reg.orders_simulation_end.clear();
    reg.orders_with_pending_nonces.clear();
}

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
        | ReplaceableOrderPoolCommand::CancelBundle(_) => "replacement",
    };
    ORDERPOOL_ORDERS_RECEIVED.with_label_values(&[kind]).inc();
}

fn mark_order_received(id: OrderId, received_at: OffsetDateTime) {
    let mut reg = lock_registry();

    let timestamp = offset_datetime_to_timestamp_us(&received_at);
    if !timestamp_in_critical_period(
        timestamp,
        reg.last_slot_critical_period_start,
        reg.last_slot_critical_period_end,
    ) {
        return;
    }

    if reg.orders_received.contains_key(&id) {
        return;
    }
    reg.orders_received.insert(id, timestamp);
}

pub fn mark_order_pending_nonce(id: OrderId) {
    let mut reg = lock_registry();

    let now = timestamp_now_us();
    if !timestamp_in_critical_period(
        now,
        reg.last_slot_critical_period_start,
        reg.last_slot_critical_period_end,
    ) {
        return;
    }

    reg.orders_with_pending_nonces.insert(id);
}

pub fn mark_order_simulation_end(id: OrderId, success: bool) {
    let mut reg = lock_registry();

    let now = timestamp_now_us();
    if !timestamp_in_critical_period(
        now,
        reg.last_slot_critical_period_start,
        reg.last_slot_critical_period_end,
    ) {
        return;
    }

    let received_at = if let Some(ts) = reg.orders_received.get(&id) {
        *ts
    } else {
        return;
    };

    if reg.orders_simulation_end.contains_key(&id) {
        return;
    }

    let now = timestamp_now_us();

    reg.orders_simulation_end.insert(id, now);

    // we con't record metrics for ordrers that were stuck due to nonce
    if reg.orders_with_pending_nonces.contains(&id) {
        return;
    }

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

fn offset_datetime_to_timestamp_us(dt: &OffsetDateTime) -> Timestamp {
    (dt.unix_timestamp_nanos() / 1_000)
        .try_into()
        .unwrap_or_default()
}

fn timestamp_now_us() -> Timestamp {
    offset_datetime_to_timestamp_us(&OffsetDateTime::now_utc())
}

fn timestamp_in_critical_period(time: Timestamp, start: Timestamp, end: Timestamp) -> bool {
    let too_early = time < start;
    let too_late = time > end;
    !too_early && !too_late
}
