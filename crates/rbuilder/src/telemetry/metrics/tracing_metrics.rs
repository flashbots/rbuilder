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
    ORDER_SIM_END_TO_FIRST_BUILD_STARTED_MIN_TIME, ORDER_SIM_END_TO_FIRST_BUILD_STARTED_TIME,
};

type Timestamp = u64; // timestamp in microseconds
type BuilderId = u64; // integer id to minimize string cloning

#[derive(Debug, Default)]
struct TracingMetricsData {
    last_slot_critical_period_start: Timestamp,
    last_slot_critical_period_end: Timestamp,

    builder_by_name: HashMap<String, u64>,

    // All fields below must be cleaned once per slot in `mark_building_started`
    orders_received: HashMap<OrderId, Timestamp>,
    orders_with_pending_nonces: HashSet<OrderId>,
    orders_simulation_end: HashMap<OrderId, Timestamp>,

    orders_not_ready_for_immediate_inclusion: HashSet<OrderId>,
    orders_first_insertion_block_seal_start_by_builder: HashMap<(OrderId, BuilderId), Timestamp>,
    orders_first_insertion_block_seal_start: HashMap<OrderId, (Timestamp, BuilderId)>,
}

impl TracingMetricsData {
    fn get_builder_id(&mut self, name: &str) -> BuilderId {
        if let Some(id) = self.builder_by_name.get(name) {
            return *id;
        }
        let id = self.builder_by_name.len() as u64;
        self.builder_by_name.insert(name.to_string(), id);
        id
    }
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
    reg.orders_first_insertion_block_seal_start_by_builder
        .clear();
    reg.orders_first_insertion_block_seal_start.clear();
    reg.orders_not_ready_for_immediate_inclusion.clear();
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

pub fn mark_order_not_ready_for_immediate_inclusion(order_id: &OrderId) {
    let mut reg = lock_registry();
    if reg
        .orders_not_ready_for_immediate_inclusion
        .contains(order_id)
    {
        return;
    };
    reg.orders_not_ready_for_immediate_inclusion
        .insert(order_id.clone());
}

pub fn mark_builder_considering_order(
    order_id: OrderId,
    order_closed_at: &OffsetDateTime,
    builder_name: &str,
) {
    let mut reg = lock_registry();

    let timestamp = offset_datetime_to_timestamp_us(order_closed_at);
    if !timestamp_in_critical_period(
        timestamp,
        reg.last_slot_critical_period_start,
        reg.last_slot_critical_period_end,
    ) {
        return;
    }

    let builder_id = reg.get_builder_id(builder_name);
    if reg
        .orders_first_insertion_block_seal_start_by_builder
        .contains_key(&(order_id, builder_id))
    {
        return;
    }

    let order_sim_end_time = reg
        .orders_simulation_end
        .get(&order_id)
        .cloned()
        .unwrap_or_default();
    let ready_for_immediate_inclusion = reg
        .orders_not_ready_for_immediate_inclusion
        .contains(&order_id);

    let min_time_set = if !reg
        .orders_first_insertion_block_seal_start
        .contains_key(&order_id)
    {
        reg.orders_first_insertion_block_seal_start
            .insert(order_id.clone(), (builder_id, timestamp));
        true
    } else {
        false
    };

    reg.orders_first_insertion_block_seal_start_by_builder
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

fn timestamp_now_us() -> Timestamp {
    offset_datetime_to_timestamp_us(&OffsetDateTime::now_utc())
}

fn timestamp_in_critical_period(time: Timestamp, start: Timestamp, end: Timestamp) -> bool {
    let too_early = time < start;
    let too_late = time > end;
    !too_early && !too_late
}
