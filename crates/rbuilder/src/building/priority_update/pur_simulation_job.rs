use std::sync::Arc;

use ahash::HashMap;
use parking_lot::Mutex;
use rbuilder_primitives::{Order, OrderId, SimulatedOrder};
use rbuilder_utils::replace_event_scheduler::ReplaceEventSchedulerSubscription;
use reth_provider::StateProvider;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, trace};
use uuid::Uuid;

use crate::{
    building::{BlockBuildingContext, ThreadBlockBuildingContext},
    live_builder::simulation::{simulation_job_tracer::SimulationJobTracer, SimulatedOrderCommand},
    provider::StateProviderFactory,
};

use super::{simulate::simulate_priority_update, PriorityUpdatePool};

/// Capacity of each per-subscriber channel returned from
/// [`PUSimulationContext::subscribe`]. Matches the builder pipeline channel.
const PU_SUBSCRIBER_CHANNEL_CAPACITY: usize = 10_000;

/// Upper bound on PU updates a sim worker drains per loop iteration.
const PU_BATCH_DRAIN_LIMIT: usize = 256;

/// Shared inner state of the priority-update pool plus the fan-out subscriber
/// list.
#[derive(Debug)]
struct Inner {
    pool: PriorityUpdatePool,
    subscribers: Vec<mpsc::Sender<SimulatedOrderCommand>>,
}

/// Public handle stored in `SimulationContext`. Sim workers use this to obtain
/// an atomic `(snapshot, channel)` pair: the pool snapshot reflects all updates
/// seen so far, the channel will deliver every update made afterwards.
#[derive(Clone, Debug)]
pub struct PUSimulationContext {
    inner: Arc<Mutex<Inner>>,
}

impl PUSimulationContext {
    pub fn subscribe(&self) -> PUSimWorkerOrderpool {
        let (tx, rx) = mpsc::channel(PU_SUBSCRIBER_CHANNEL_CAPACITY);
        let mut g = self.inner.lock();
        let pool = g.pool.clone();
        g.subscribers.push(tx);
        PUSimWorkerOrderpool {
            pool: Arc::new(Mutex::new(pool)),
            receiver: rx,
        }
    }
}

/// Per-sim-worker pool plus the channel that feeds it. Owns the shared pool
/// handle the worker hands to the overlay [`crate::building::BlockState`].
pub struct PUSimWorkerOrderpool {
    pool: Arc<Mutex<PriorityUpdatePool>>,
    receiver: mpsc::Receiver<SimulatedOrderCommand>,
}

impl PUSimWorkerOrderpool {
    /// Shared handle to the current PU orderpool state.
    pub fn pool(&self) -> Arc<Mutex<PriorityUpdatePool>> {
        Arc::clone(&self.pool)
    }

    /// Drain a bounded batch of pending updates from the channel into the pool.
    pub fn consume_updates(&mut self) {
        let mut pool = self.pool.lock();
        for _ in 0..PU_BATCH_DRAIN_LIMIT {
            match self.receiver.try_recv() {
                Ok(SimulatedOrderCommand::Simulation(sim_order)) => {
                    if sim_order.pu_data.is_some() {
                        pool.apply_update(sim_order);
                    }
                }
                Ok(SimulatedOrderCommand::Cancellation(id)) => {
                    pool.apply_remove(&id);
                }
                Err(mpsc::error::TryRecvError::Empty)
                | Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    }
}

pub struct PUSimulationWorkerState {
    inner: Arc<Mutex<Inner>>,
}

impl PUSimulationWorkerState {
    async fn apply_update(&self, sim_order: Arc<SimulatedOrder>) -> Vec<OrderId> {
        // Sync critical section: mutate pool, prune closed subs, snapshot subs.
        let (evicted, subs) = {
            let mut g = self.inner.lock();
            let evicted = g.pool.apply_update(Arc::clone(&sim_order));
            g.subscribers.retain(|s| !s.is_closed());
            (evicted, g.subscribers.clone())
        };
        let cmd = SimulatedOrderCommand::Simulation(sim_order);
        for sub in subs {
            let _ = sub.send(cmd.clone()).await;
        }
        evicted
    }

    async fn apply_remove(&self, order_id: OrderId) {
        let subs = {
            let mut g = self.inner.lock();
            g.pool.apply_remove(&order_id);
            g.subscribers.retain(|s| !s.is_closed());
            g.subscribers.clone()
        };
        let cmd = SimulatedOrderCommand::Cancellation(order_id);
        for sub in subs {
            let _ = sub.send(cmd.clone()).await;
        }
    }
}

/// Builds the shared inner state, pre-registering `builder_sender` as the
/// always-on subscriber that feeds the main pipeline.
pub fn new_pu_simulation_runtime(
    builder_sender: mpsc::Sender<SimulatedOrderCommand>,
) -> (PUSimulationContext, PUSimulationWorkerState) {
    let inner = Arc::new(Mutex::new(Inner {
        pool: PriorityUpdatePool::new(),
        subscribers: vec![builder_sender],
    }));
    (
        PUSimulationContext {
            inner: inner.clone(),
        },
        PUSimulationWorkerState { inner },
    )
}

/// Drives the priority-update simulation thread for one block.
pub async fn run_pur_sim_worker<P>(
    provider: P,
    block_ctx: BlockBuildingContext,
    subscription: ReplaceEventSchedulerSubscription<Uuid, Option<Arc<Order>>>,
    state: PUSimulationWorkerState,
    block_cancellation: CancellationToken,
    sim_tracer: Arc<dyn SimulationJobTracer>,
) where
    P: StateProviderFactory,
{
    let parent_state: Arc<dyn StateProvider> =
        match provider.history_by_block_hash(block_ctx.attributes.parent) {
            Ok(state) => Arc::from(state),
            Err(err) => {
                error!(?err, "PUR worker: failed to get parent state provider");
                return;
            }
        };

    let mut local_ctx = ThreadBlockBuildingContext::default();
    let mut active: HashMap<Uuid, OrderId> = HashMap::default();
    let mut buf: Vec<(Uuid, Option<Arc<Order>>)> = Vec::new();

    loop {
        buf.clear();
        subscription.pop_unprocessed_events(PU_BATCH_DRAIN_LIMIT, &mut buf);
        for (uuid, maybe_order) in buf.drain(..) {
            process_event(
                uuid,
                maybe_order,
                &block_ctx,
                &mut local_ctx,
                &parent_state,
                &state,
                &sim_tracer,
                &mut active,
            )
            .await;
        }

        tokio::select! {
            _ = block_cancellation.cancelled() => return,
            _ = subscription.notified() => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_event(
    uuid: Uuid,
    maybe_order: Option<Arc<Order>>,
    block_ctx: &BlockBuildingContext,
    local_ctx: &mut ThreadBlockBuildingContext,
    parent_state: &Arc<dyn StateProvider>,
    state: &PUSimulationWorkerState,
    sim_tracer: &Arc<dyn SimulationJobTracer>,
    active: &mut HashMap<Uuid, OrderId>,
) {
    match maybe_order {
        Some(order) => {
            let order_id = order.id();
            let sim_res = simulate_priority_update(
                Arc::clone(&order),
                block_ctx,
                local_ctx,
                Arc::clone(parent_state),
            );
            let simulated_order = match sim_res {
                Ok(Some(res)) => {
                    trace!(?order_id, ?uuid, success = true, "PU simulated");
                    res
                }
                Ok(None) => {
                    trace!(?order_id, ?uuid, success = false, "PU simulated");
                    return;
                }
                Err(err) => {
                    trace!(?order_id, ?uuid, success = false, ?err, "PU simulated");
                    return;
                }
            };

            // New version supersedes the old: drop the previous OrderId for this uuid first.
            if let Some(prev_id) = active.remove(&uuid) {
                state.apply_remove(prev_id).await;
                sim_tracer.update_cancellation_sent(&prev_id);
            }
            let evicted = state.apply_update(Arc::clone(&simulated_order)).await;
            for evicted_id in &evicted {
                trace!(order_id = ?evicted_id, reason = "conflicting", "PU removed");
            }
            active.insert(uuid, order_id);
        }
        None => {
            if let Some(prev_id) = active.remove(&uuid) {
                trace!(order_id = ?prev_id, ?uuid, reason = "cancelled", "PU removed");
                state.apply_remove(prev_id).await;
                sim_tracer.update_cancellation_sent(&prev_id);
            }
        }
    }
}
