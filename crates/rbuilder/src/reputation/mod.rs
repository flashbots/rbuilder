use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, LazyLock,
    },
    time::{Duration, Instant},
};

use alloy_consensus::constants::GWEI_TO_WEI;
use alloy_primitives::{Address, U256};
use dashmap::DashMap;
use tracing::info;

use crate::{
    building::{ExecutionError, ExecutionResult},
    primitives::{Order, SimulatedOrder},
};

static STORAGE: LazyLock<ReputationStorage> = LazyLock::new(|| ReputationStorage::default());

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReputationKey {
    signer: Option<Address>,
    first_tx_to: Option<Address>,
}

#[derive(Debug, Default)]
struct ReputationCounters {
    value_paid_to_coinbase: AtomicU64,
    time_spent_simulating: AtomicU64,
    simulation_counts: AtomicU64,
    is_high_priority: AtomicBool,
}

#[derive(Debug, Default)]
struct ReputationStorage {
    data: Arc<DashMap<ReputationKey, Arc<ReputationCounters>>>,
}

pub fn reputation_block_start() {
    let start = Instant::now();
    let mut scores = Vec::new();
    let reputation_keys = STORAGE.data.len();
    for data in STORAGE.data.iter() {
        let value_paid = data.value_paid_to_coinbase.load(Ordering::Relaxed);
        let time_spent = data.time_spent_simulating.load(Ordering::Relaxed);
        let sim_counts = data.simulation_counts.load(Ordering::Relaxed);
        if value_paid == 0 || time_spent == 0 || sim_counts == 0 {
            continue;
        }
        let score = (value_paid as f64 / time_spent as f64) / sim_counts as f64;
        scores.push(score);
    }
    scores.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if scores.len() < 100 {
        return;
    }
    let percentile_idx = (75 * scores.len()) / 100;
    let high_priority_score = scores[percentile_idx];

    let mut high_priority_keys = 0;
    for data in STORAGE.data.iter() {
        let value_paid = data.value_paid_to_coinbase.load(Ordering::Relaxed);
        let time_spent = data.time_spent_simulating.load(Ordering::Relaxed);
        let sim_counts = data.simulation_counts.load(Ordering::Relaxed);
        let high_prio = if value_paid == 0 || time_spent == 0 || sim_counts == 0 {
            false
        } else {
            let score = (value_paid as f64 / time_spent as f64) / sim_counts as f64;
            score >= high_priority_score
        };
        if high_prio {
            high_priority_keys += 1;
        }
        data.is_high_priority.store(high_prio, Ordering::Relaxed);
    }

    info!(
        high_priority_keys,
        reputation_keys,
        time_ms = start.elapsed().as_millis(),
        "Reputation storage updated"
    );
}

fn key(order: &Order) -> ReputationKey {
    let signer = order.signer();
    let first_tx_to = {
        let txs = order.list_txs();
        if txs.len() == 1 {
            txs[0].0.to()
        } else {
            None
        }
    };
    ReputationKey {
        signer,
        first_tx_to,
    }
}

pub fn reputation_is_order_high_priority(order: &Order) -> bool {
    let key = key(order);
    if let Some(data) = STORAGE.data.get(&key) {
        data.is_high_priority.load(Ordering::Relaxed)
    } else {
        false
    }
}

pub fn reputation_add_order_simulation_result(
    order: &SimulatedOrder,
    result: &Result<&ExecutionResult, ExecutionError>,
    duration: Duration,
) {
    let key = key(&order.order);
    let value_paid_to_coinbase = match result {
        Ok(res) => res.coinbase_profit,
        Err(_) => Default::default(),
    };

    let value_paid_to_coinbase: u64 = value_paid_to_coinbase
        .checked_div(U256::from(GWEI_TO_WEI))
        .map(|uint| uint.try_into().unwrap_or_default())
        .unwrap_or_default();
    let time_spent_simulating = duration.as_micros() as u64;

    let counters = if let Some(counters) = STORAGE.data.get(&key) {
        counters.clone()
    } else {
        STORAGE.data.entry(key).or_default().clone()
    };

    counters
        .value_paid_to_coinbase
        .fetch_add(value_paid_to_coinbase, Ordering::Relaxed);
    counters
        .time_spent_simulating
        .fetch_add(time_spent_simulating, Ordering::Relaxed);
    counters.simulation_counts.fetch_add(1, Ordering::Relaxed);
}
