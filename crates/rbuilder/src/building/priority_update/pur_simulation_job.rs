use std::sync::Arc;

use ahash::HashSet;
use parking_lot::Mutex;
use rbuilder_primitives::{Order, OrderId, SimulatedOrder};
use reth_provider::StateProvider;
use tokio::sync::mpsc::{self, error::TryRecvError};
use tokio_util::sync::CancellationToken;
use tracing::{error, trace};

use crate::{
    building::{BlockBuildingContext, ThreadBlockBuildingContext},
    live_builder::{
        order_input::order_sink::OrderPoolCommand,
        simulation::{simulation_job_tracer::SimulationJobTracer, SimulatedOrderCommand},
    },
    provider::StateProviderFactory,
};

use super::{simulate::simulate_priority_update, PriorityUpdatePool};

/// Capacity of each per-subscriber channel returned from
/// [`PUSimulationContext::subscribe`]. Matches the builder pipeline channel.
const PU_SUBSCRIBER_CHANNEL_CAPACITY: usize = 10_000;

/// Upper bound on PU messages a sim worker drains per call to
/// [`PUSimWorkerOrderpool::consume_updates`].
const PU_BATCH_DRAIN_LIMIT: usize = 256;

// TODO: implement real classification logic.
pub fn is_priority_update(order: &Order) -> bool {
    let _ = order;
    false
}

#[derive(Debug)]
struct ClassifierInner {
    cmd_sender: mpsc::UnboundedSender<OrderPoolCommand>,
    tracked_orders: Mutex<HashSet<OrderId>>,
}

/// Classifier shared with [`SimulationJob`]: PU-classified commands are
/// forwarded to the PUR sim thread and swallowed from the main pipeline.
#[derive(Clone, Debug)]
pub struct PURCommandClassifier {
    inner: Arc<ClassifierInner>,
}

/// Receiver side handed to the PUR sim thread.
pub struct PURSimulationInput {
    cmd_receiver: mpsc::UnboundedReceiver<OrderPoolCommand>,
}

pub fn new_pur_simulation_channel() -> (PURCommandClassifier, PURSimulationInput) {
    let (cmd_sender, cmd_receiver) = mpsc::unbounded_channel();
    let classifier = PURCommandClassifier {
        inner: Arc::new(ClassifierInner {
            cmd_sender,
            tracked_orders: Mutex::new(HashSet::default()),
        }),
    };
    let input = PURSimulationInput { cmd_receiver };
    (classifier, input)
}

impl PURCommandClassifier {
    pub fn try_consuming_new_order_command(&self, cmd: &OrderPoolCommand) -> bool {
        match cmd {
            OrderPoolCommand::Insert(order) => {
                if !is_priority_update(order.as_ref()) {
                    return false;
                }
                self.inner.tracked_orders.lock().insert(order.id());
                let _ = self
                    .inner
                    .cmd_sender
                    .send(OrderPoolCommand::Insert(Arc::clone(order)));
                true
            }
            OrderPoolCommand::Remove(id) => {
                let known = self.inner.tracked_orders.lock().remove(id);
                if known {
                    let _ = self.inner.cmd_sender.send(OrderPoolCommand::Remove(*id));
                    true
                } else {
                    false
                }
            }
        }
    }
}

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
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
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

pub async fn run_pur_sim_worker<P>(
    provider: P,
    block_ctx: BlockBuildingContext,
    input: PURSimulationInput,
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

    let PURSimulationInput { mut cmd_receiver } = input;

    let mut local_ctx = ThreadBlockBuildingContext::default();

    loop {
        tokio::select! {
            _ = block_cancellation.cancelled() => return,
            maybe_cmd = cmd_receiver.recv() => {
                let Some(cmd) = maybe_cmd else { return; };
                process_command(
                    cmd,
                    &block_ctx,
                    &mut local_ctx,
                    &parent_state,
                    &state,
                    &sim_tracer,
                )
                .await;
            }
        }
    }
}

async fn process_command(
    cmd: OrderPoolCommand,
    block_ctx: &BlockBuildingContext,
    local_ctx: &mut ThreadBlockBuildingContext,
    parent_state: &Arc<dyn StateProvider>,
    state: &PUSimulationWorkerState,
    sim_tracer: &Arc<dyn SimulationJobTracer>,
) {
    match cmd {
        OrderPoolCommand::Insert(order) => {
            let order_id = order.id();

            let sim_res = simulate_priority_update(
                Arc::clone(&order),
                block_ctx,
                local_ctx,
                Arc::clone(parent_state),
            );

            let simulated_order = match sim_res {
                Ok(Some(res)) => {
                    trace!(?order_id, success = true, "PU simulated");
                    res
                }
                Ok(None) => {
                    trace!(?order_id, success = false, "PU simulated");
                    return;
                }
                Err(err) => {
                    trace!(?order_id, success = false, ?err, "PU simulated");
                    return;
                }
            };

            let evicted = state.apply_update(Arc::clone(&simulated_order)).await;
            for evicted_id in &evicted {
                trace!(order_id = ?evicted_id, reason = "conflicting", "PU removed");
            }
        }
        OrderPoolCommand::Remove(order_id) => {
            trace!(?order_id, reason = "cancelled", "PU removed");
            state.apply_remove(order_id).await;
            sim_tracer.update_cancellation_sent(&order_id);
        }
    }
}
