use std::{iter, time::Instant};

use ahash::{HashMap, HashSet};
use alloy_primitives::{Address, B256};
use eth_sparse_mpt::{
    prefetch_tries_for_accounts,
    reth_sparse_trie::{trie_fetcher::FetchNodeError, SparseTrieError, SparseTrieSharedCache},
    ChangedAccountData,
};
use reth::providers::providers::ConsistentDbView;
use reth_db::database::Database;
use reth_errors::ProviderError;
use reth_provider::DatabaseProviderFactory;
use tokio::sync::broadcast::{
    self,
    error::{RecvError, TryRecvError},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, trace, warn};

use crate::{
    building::evm_inspector::SlotKey, live_builder::simulation::SimulatedOrderCommand,
    primitives::SimulatedOrder,
};

const CONSUME_SIM_ORDERS_BATCH: usize = 128;

/// Runs a process that prefetches pieces of the trie based on the slots used by the order in simulation
/// Its a blocking call so it should be spawned on the separate thread.
pub fn run_trie_prefetcher<P, DB>(
    parent_hash: B256,
    shared_sparse_mpt_cache: SparseTrieSharedCache,
    provider: P,
    mut simulated_orders: broadcast::Receiver<SimulatedOrderCommand>,
    cancel: CancellationToken,
) where
    P: DatabaseProviderFactory<DB> + Clone + Send + Sync,
    DB: Database + Clone + 'static,
{
    let consistent_db_view = ConsistentDbView::new(provider, Some(parent_hash));

    // here we mark data that was fetched for this slot before
    let mut fetched_accounts: HashSet<Address> = HashSet::default();
    let mut fetched_slots: HashSet<SlotKey> = HashSet::default();

    // loop local variables
    let mut used_state_traces = Vec::new();
    let mut fetch_request: HashMap<Address, ChangedAccountData> = HashMap::default();
    loop {
        used_state_traces.clear();
        fetch_request.clear();

        if cancel.is_cancelled() {
            return;
        }

        for _ in 0..CONSUME_SIM_ORDERS_BATCH {
            match simulated_orders.try_recv() {
                Ok(SimulatedOrderCommand::Simulation(SimulatedOrder {
                    used_state_trace: Some(used_state_trace),
                    ..
                })) => {
                    used_state_traces.push(used_state_trace);
                }
                Ok(_) => continue,
                Err(TryRecvError::Empty) => {
                    if !used_state_traces.is_empty() {
                        break;
                    }
                    // block so thread can sleep if there are no inputs
                    match simulated_orders.blocking_recv() {
                        Ok(SimulatedOrderCommand::Simulation(SimulatedOrder {
                            used_state_trace: Some(used_state_trace),
                            ..
                        })) => {
                            used_state_traces.push(used_state_trace);
                        }
                        Ok(_) => continue,
                        Err(RecvError::Closed) => return,
                        Err(RecvError::Lagged(msg)) => {
                            warn!(
                                "State trie prefetching thread lagging on sim orders channel: {}",
                                msg
                            );
                            break;
                        }
                    }
                }
                Err(TryRecvError::Closed) => {
                    return;
                }
                Err(TryRecvError::Lagged(msg)) => {
                    warn!(
                        "State trie prefetching thread lagging on sim orders channel: {}",
                        msg
                    );
                    break;
                }
            };
        }

        for used_state_trace in used_state_traces.drain(..) {
            let changed_accounts_iter = used_state_trace
                .received_amount
                .keys()
                .chain(used_state_trace.sent_amount.keys())
                .zip(iter::repeat(false))
                .chain(
                    used_state_trace
                        .destructed_contracts
                        .iter()
                        .zip(iter::repeat(true)),
                );

            for (address, destroyed) in changed_accounts_iter {
                if fetched_accounts.contains(address) {
                    continue;
                }
                fetched_accounts.insert(*address);
                fetch_request
                    .entry(*address)
                    .or_insert_with(|| ChangedAccountData::new(*address, destroyed));
            }

            for (written_slot, value) in &used_state_trace.written_slot_values {
                if fetched_slots.contains(written_slot) {
                    continue;
                }
                fetched_slots.insert(written_slot.clone());
                let account_request = fetch_request
                    .entry(written_slot.address)
                    .or_insert_with(|| ChangedAccountData::new(written_slot.address, false));
                account_request
                    .slots
                    .push((written_slot.key, value.is_zero()));
            }
        }

        if fetch_request.is_empty() {
            continue;
        }

        let start = Instant::now();
        match prefetch_tries_for_accounts(
            consistent_db_view.clone(),
            shared_sparse_mpt_cache.clone(),
            fetch_request.values(),
        ) {
            Ok(()) => {}
            Err(SparseTrieError::FetchNode(FetchNodeError::Provider(
                ProviderError::ConsistentView(_),
            ))) => {
                return;
            }
            Err(err) => {
                error!(?err, "Error while prefetching trie nodes");
            }
        }
        trace!(
            time_ms = start.elapsed().as_millis(),
            accounts = fetch_request.len(),
            "Prefetched trie nodes"
        );
    }
}
