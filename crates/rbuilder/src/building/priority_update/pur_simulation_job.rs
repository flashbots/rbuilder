use std::sync::Arc;

use ahash::HashMap;
use parking_lot::Mutex;
use rbuilder_primitives::{Order, OrderId, SimulatedOrder};
use rbuilder_utils::replace_event_scheduler::{
    ReplaceEventObserver, ReplaceEventScheduler, ReplaceEventSchedulerSubscription,
};
use reth_provider::StateProvider;
use tokio_util::sync::CancellationToken;
use tracing::{error, trace};
use uuid::Uuid;

use crate::{
    building::{
        journal::{
            JournalLane, JournalSequenceNumber, OrderJournalObserver, SimulatedOrderJournalCommand,
        },
        BlockBuildingContext, ThreadBlockBuildingContext,
    },
    live_builder::simulation::SimulatedOrderCommand,
    provider::StateProviderFactory,
};

use super::{simulate::simulate_priority_update, PriorityUpdatePool};

/// Upper bound on PU updates a worker drains per loop iteration.
const PU_BATCH_DRAIN_LIMIT: usize = 256;

/// Subscription type produced by [`PUSimulationContext::subscribe`].
pub type PUResultSubscription =
    ReplaceEventSchedulerSubscription<Uuid, Option<Arc<SimulatedOrder>>>;

/// Adapter that turns each successful add on the PU result scheduler into
/// `SimulatedOrderJournalCommand`s on the [`JournalLane::Pu`] lane and forwards
/// them to the per-slot `OrderJournalObserver`.
///
/// Owns the lane's monotonic sequence counter (starting at 0) and the sole
/// `uuid → OrderId` map needed to translate `None` values and same-uuid
/// replacements into concrete `Cancellation` commands.
#[derive(Debug)]
pub struct PuLaneObserver {
    journal: Arc<dyn OrderJournalObserver + Send + Sync>,
    state: Mutex<PuLaneState>,
}

#[derive(Debug, Default)]
struct PuLaneState {
    active: HashMap<Uuid, OrderId>,
    next_seq: JournalSequenceNumber,
}

impl PuLaneObserver {
    pub fn new(journal: Arc<dyn OrderJournalObserver + Send + Sync>) -> Self {
        Self {
            journal,
            state: Mutex::new(PuLaneState::default()),
        }
    }

    fn deliver(&self, state: &mut PuLaneState, command: SimulatedOrderCommand) {
        let journal_command =
            SimulatedOrderJournalCommand::new(command, state.next_seq, JournalLane::Pu);
        state.next_seq += 1;
        self.journal.order_delivered(&journal_command);
    }
}

impl ReplaceEventObserver<Uuid, Option<Arc<SimulatedOrder>>> for PuLaneObserver {
    fn on_event(&self, uuid: &Uuid, _seq: u64, value: &Option<Arc<SimulatedOrder>>) {
        let mut state = self.state.lock();
        match value {
            Some(sim) => {
                let new_id = sim.id();
                if let Some(prev_id) = state.active.insert(*uuid, new_id) {
                    if prev_id == new_id {
                        return;
                    }
                    self.deliver(&mut state, SimulatedOrderCommand::Cancellation(prev_id));
                }
                self.deliver(
                    &mut state,
                    SimulatedOrderCommand::Simulation(Arc::clone(sim)),
                );
            }
            None => {
                if let Some(prev_id) = state.active.remove(uuid) {
                    self.deliver(&mut state, SimulatedOrderCommand::Cancellation(prev_id));
                }
            }
        }
    }
}

#[derive(Debug)]
struct Inner {
    pool: PriorityUpdatePool,
    scheduler: ReplaceEventScheduler<Uuid, Option<Arc<SimulatedOrder>>, PuLaneObserver>,
}

/// Public handle stored in `SimulationContext`. Sim workers and the
/// journal-bridge thread call [`Self::subscribe`] to receive coalesced
/// `(uuid, Option<SimulatedOrder>)` updates as the PUR worker produces them.
#[derive(Clone, Debug)]
pub struct PUSimulationContext {
    inner: Arc<Mutex<Inner>>,
}

impl PUSimulationContext {
    pub fn subscribe(&self) -> PUResultSubscription {
        self.inner.lock().scheduler.subscribe()
    }
}

/// Worker-side handle owned by the PUR simulation thread. Mutates the local
/// pool and pushes results to the scheduler.
pub struct PUSimulationWorkerState {
    inner: Arc<Mutex<Inner>>,
}

impl PUSimulationWorkerState {
    /// Apply a fresh sim result for `uuid` to the local pool and publish it
    /// under the caller-supplied `seq` (the input scheduler's per-uuid seq).
    /// Storage-conflict evictions are emitted as `(evicted_uuid, evicted_seq,
    /// None)` reusing the seq the evicted entry was originally added with —
    /// the scheduler's same-seq replacement rule lets the `None` overwrite the
    /// prior stored `Some`.
    pub fn submit_update(&self, uuid: Uuid, seq: u64, sim_order: Arc<SimulatedOrder>) {
        let mut g = self.inner.lock();
        let evicted = g.pool.apply_event(uuid, seq, Some(Arc::clone(&sim_order)));
        for (evicted_uuid, evicted_seq) in evicted {
            g.scheduler.add_event(evicted_uuid, evicted_seq, None);
        }
        g.scheduler.add_event(uuid, seq, Some(sim_order));
    }

    /// Cancel the active sim for `uuid`, publishing `(uuid, seq, None)`.
    /// `seq` is the input scheduler's per-uuid seq for the cancellation event.
    pub fn submit_cancel(&self, uuid: Uuid, seq: u64) {
        let mut g = self.inner.lock();
        let _ = g.pool.apply_event(uuid, seq, None);
        g.scheduler.add_event(uuid, seq, None);
    }
}

/// Builds a fresh per-block PU simulation runtime. The `journal` observer is
/// attached to the result scheduler so its `pu_event` hook fires for every
/// PUR-produced add, with the scheduler's per-uuid seq used as the Pu lane's
/// journal sequence number.
pub fn new_pu_simulation_runtime(
    journal: Arc<dyn OrderJournalObserver + Send + Sync>,
) -> (PUSimulationContext, PUSimulationWorkerState) {
    let scheduler = ReplaceEventScheduler::with_observer(PuLaneObserver::new(journal));
    let inner = Arc::new(Mutex::new(Inner {
        pool: PriorityUpdatePool::new(),
        scheduler,
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
    let mut buf: Vec<(Uuid, u64, Option<Arc<Order>>)> = Vec::new();

    loop {
        buf.clear();
        subscription.pop_unprocessed_events(PU_BATCH_DRAIN_LIMIT, &mut buf);
        for (uuid, seq, maybe_order) in buf.drain(..) {
            process_event(
                uuid,
                seq,
                maybe_order,
                &block_ctx,
                &mut local_ctx,
                &parent_state,
                &state,
            );
        }

        tokio::select! {
            _ = block_cancellation.cancelled() => return,
            _ = subscription.notified() => {}
        }
    }
}

fn process_event(
    uuid: Uuid,
    seq: u64,
    maybe_order: Option<Arc<Order>>,
    block_ctx: &BlockBuildingContext,
    local_ctx: &mut ThreadBlockBuildingContext,
    parent_state: &Arc<dyn StateProvider>,
    state: &PUSimulationWorkerState,
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
            state.submit_update(uuid, seq, simulated_order);
        }
        None => {
            trace!(?uuid, reason = "cancelled", "PU removed");
            state.submit_cancel(uuid, seq);
        }
    }
}
