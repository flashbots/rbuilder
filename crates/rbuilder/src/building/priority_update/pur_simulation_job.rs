use std::sync::Arc;

use ahash::HashSet;
use parking_lot::{ArcRwLockReadGuard, Mutex, RawRwLock, RwLock};
use rbuilder_primitives::{Order, OrderId};
use reth_provider::StateProvider;
use tokio::sync::mpsc;
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

use super::{
    pending_updates::PendingUpdates,
    simulate::{simulate_priority_update, PurSimulationResult},
};

// TODO: implement real classification logic.
pub fn is_priority_update(order: &Order) -> bool {
    let _ = order;
    false
}

#[derive(Debug)]
struct Inner {
    pending: Arc<RwLock<PendingUpdates>>,
    cmd_sender: mpsc::UnboundedSender<OrderPoolCommand>,
    tracked_orders: Mutex<HashSet<OrderId>>,
}

#[derive(Clone, Debug)]
pub struct PURSimulationStateExternal {
    inner: Arc<Inner>,
}

pub struct PURSimulationStateForPURSimThread {
    pending: Arc<RwLock<PendingUpdates>>,
    cmd_receiver: mpsc::UnboundedReceiver<OrderPoolCommand>,
}

pub fn new_pur_simulation_state() -> (
    PURSimulationStateExternal,
    PURSimulationStateForPURSimThread,
) {
    let (cmd_sender, cmd_receiver) = mpsc::unbounded_channel();
    let pending = Arc::new(RwLock::new(PendingUpdates::new()));
    let external = PURSimulationStateExternal {
        inner: Arc::new(Inner {
            pending: Arc::clone(&pending),
            cmd_sender,
            tracked_orders: Mutex::new(HashSet::default()),
        }),
    };
    let internal = PURSimulationStateForPURSimThread {
        pending,
        cmd_receiver,
    };
    (external, internal)
}

impl PURSimulationStateExternal {
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

    /// Holds a read lock on the overlay for the lifetime of the returned guard.
    pub fn get_state_overlay_read_lock(&self) -> ArcRwLockReadGuard<RawRwLock, PendingUpdates> {
        self.inner.pending.read_arc()
    }
}

pub async fn run_pur_sim_worker<P>(
    provider: P,
    block_ctx: BlockBuildingContext,
    state: PURSimulationStateForPURSimThread,
    out_sender: mpsc::Sender<SimulatedOrderCommand>,
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

    let PURSimulationStateForPURSimThread {
        pending,
        mut cmd_receiver,
    } = state;

    let mut local_ctx = ThreadBlockBuildingContext::default();
    let mut emitted: HashSet<OrderId> = HashSet::default();

    loop {
        tokio::select! {
            _ = block_cancellation.cancelled() => return,
            maybe_cmd = cmd_receiver.recv() => {
                let Some(cmd) = maybe_cmd else { return; };
                if !process_command(
                    cmd,
                    &block_ctx,
                    &mut local_ctx,
                    &parent_state,
                    &pending,
                    &out_sender,
                    &mut emitted,
                    &sim_tracer,
                )
                .await
                {
                    return;
                }
            }
        }
    }
}

async fn process_command(
    cmd: OrderPoolCommand,
    block_ctx: &BlockBuildingContext,
    local_ctx: &mut ThreadBlockBuildingContext,
    parent_state: &Arc<dyn StateProvider>,
    pending: &Arc<RwLock<PendingUpdates>>,
    out_sender: &mpsc::Sender<SimulatedOrderCommand>,
    emitted: &mut HashSet<OrderId>,
    sim_tracer: &Arc<dyn SimulationJobTracer>,
) -> bool {
    match cmd {
        OrderPoolCommand::Insert(order) => {
            let order_id = order.id();

            let sim_res = simulate_priority_update(
                Arc::clone(&order),
                block_ctx,
                local_ctx,
                Arc::clone(parent_state),
            );

            let result = match sim_res {
                Ok(Some(res)) => res,
                Ok(None) => return true,
                Err(err) => {
                    error!(?err, ?order_id, "PUR critical sim error");
                    return true;
                }
            };

            let PurSimulationResult {
                simulated_order,
                changeset,
            } = result;

            let evicted = pending
                .write()
                .add_new_simulated_update(order_id, changeset);

            for id in evicted {
                if emitted.remove(&id) {
                    if out_sender
                        .send(SimulatedOrderCommand::Cancellation(id))
                        .await
                        .is_err()
                    {
                        return false;
                    }
                    sim_tracer.update_cancellation_sent(&id);
                }
            }

            if emitted.insert(order_id) {
                trace!(?order_id, "PUR simulated, emitting downstream");
                if out_sender
                    .send(SimulatedOrderCommand::Simulation(simulated_order))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            true
        }
        OrderPoolCommand::Remove(order_id) => {
            pending.write().remove_order(&order_id);
            if emitted.remove(&order_id) {
                if out_sender
                    .send(SimulatedOrderCommand::Cancellation(order_id))
                    .await
                    .is_err()
                {
                    return false;
                }
                sim_tracer.update_cancellation_sent(&order_id);
            }
            true
        }
    }
}
