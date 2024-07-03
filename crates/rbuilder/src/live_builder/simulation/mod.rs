pub mod sim_worker;

use crate::{
    building::{
        sim::{SimTree, SimulatedResult, SimulationRequest},
        BlockBuildingContext,
    },
    live_builder::order_input::orderpool::OrdersForBlock,
    primitives::{Order, OrderId, SimulatedOrder},
    utils::{gen_uid, ProviderFactoryReopener},
};
use ahash::{HashMap, HashSet};
use alloy_primitives::utils::format_ether;
use reth_db::database::Database;
use std::{
    fmt,
    sync::{Arc, Mutex},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, info_span, trace, warn, Instrument};

use super::order_input::order_sink::OrderPoolCommand;

#[derive(Debug)]
pub struct SlotOrderSimResults {
    pub orders: mpsc::Receiver<SimulatedOrderCommand>,
}

type BlockContextId = u64;

#[derive(Debug)]
pub struct OrderSimulationPool<DB> {
    provider_factory: ProviderFactoryReopener<DB>,
    running_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    current_contexts: Arc<Mutex<CurrentSimulationContexts>>,
    worker_threads: Vec<std::thread::JoinHandle<()>>,
}

#[derive(Debug, Clone)]
pub struct SimulationContext {
    pub block_ctx: BlockBuildingContext,
    pub requests: flume::Receiver<SimulationRequest>,
    pub results: mpsc::Sender<SimulatedResult>,
}

#[derive(Debug)]
pub struct CurrentSimulationContexts {
    pub contexts: HashMap<BlockContextId, SimulationContext>,
}

#[derive(Default)]
struct OrderCounter {
    mempool_txs: usize,
    bundles: usize,
    share_bundles: usize,
}

impl fmt::Debug for OrderCounter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "OrderCounter {{ total: {}, mempool_txs: {}, bundles {}, share_bundles {} }}",
            self.total(),
            self.mempool_txs,
            self.bundles,
            self.share_bundles
        )
    }
}

impl OrderCounter {
    fn accumulate(&mut self, order: &Order) {
        match order {
            Order::Tx(_) => self.mempool_txs += 1,
            Order::Bundle(_) => self.bundles += 1,
            Order::ShareBundle(_) => self.share_bundles += 1,
        }
    }
    fn total(&self) -> usize {
        self.mempool_txs + self.bundles + self.share_bundles
    }
}

/// struct that continuously simulates orders.
/// The flow is:
/// 1 new orders are polled from new_order_sub and inserted en the SimTree
/// 2 SimTree is polled for nonce-ready orders and are sent to be simulated (sent to  sim_req_sender)
/// 3 simulation results are polled from sim_results_receiver and sent to slot_sim_results_sender
/// Cancellation flow: we add every order we start to process to in_flight_orders.
/// If we get a cancellation and the order is not in in_flight_orders we forward the cancellation.
/// If we get a cancellation and the order is in in_flight_orders we just remove it from in_flight_orders.
/// Only SimulatedOrders still in in_flight_orders are delivered.
/// @Pending: implement cancellations in the SimTree.
struct SimulationJob<DB> {
    block_cancellation: CancellationToken,
    /// Input orders to be simulated
    new_order_sub: mpsc::UnboundedReceiver<OrderPoolCommand>,
    /// Here we send requests to the simulator pool
    sim_req_sender: flume::Sender<SimulationRequest>,
    /// Here we receive the results we asked to sim_req_sender
    sim_results_receiver: mpsc::Receiver<SimulatedResult>,
    /// Output of the simulations
    slot_sim_results_sender: mpsc::Sender<SimulatedOrderCommand>,
    sim_tree: SimTree<DB>,

    pub orders_received: OrderCounter,
    pub orders_simulated_ok: OrderCounter,

    /// Orders we got via new_order_sub and are still being processed (they could be inside the SimTree or in the sim queue)
    /// and were not cancelled.
    in_flight_orders: HashSet<OrderId>,
}

#[derive(Clone, Debug)]
pub enum SimulatedOrderCommand {
    /// New or update order
    Simulation(SimulatedOrder),
    Cancellation(OrderId),
}

/// Runner of the simulations.
/// Create and call run()
/// 1- Gets the orders orders/cancellations from new_order_sub
/// 2- Using a SisTree checks when the orders re ready for simulation (nonce problems) and simulates them
/// 3- Send simulation results via sim_results_receiver
/// Also handles ShareBundle version replacements or cancellations (maybe this logic could be moved to an external object?):
/// - Always send simulations with increasing `sequence_number`
/// - When replacing an order we always send a cancellation for the previous one.
/// - When we gat a cancellation we propagate it and never again send an update.
impl<DB: Database + Clone + Send + 'static> SimulationJob<DB> {
    async fn run(&mut self) {
        let mut new_commands = Vec::new();
        let mut new_sim_results = Vec::new();
        loop {
            self.send_new_tasks_for_simulation();
            // tokio::select appears to be fair so no channel will be polled more than the other
            tokio::select! {
                n = self.new_order_sub.recv_many(&mut new_commands, 1024) => {
                    if n != 0 {
                        if !self.process_new_commands(&new_commands).await {
                            return;
                        }
                        new_commands.clear();
                    } else {
                        trace!("New order sub is closed");
                        return;
                        // channel is closed, we should cancel this job
                    }
                }
                n = self.sim_results_receiver.recv_many(&mut new_sim_results, 1024) => {
                    if n != 0 {
                        if !self.process_new_simulations(&mut new_sim_results).await {
                            return;
                        }
                        new_sim_results.clear();
                    }
                }
                _ = self.block_cancellation.cancelled() => {
                    return;
                }
            }
        }
    }

    /// Cancelled orders will return false
    fn order_still_valid(&self, order_id: &OrderId) -> bool {
        self.in_flight_orders.contains(order_id)
    }

    /// Pops tasks from SimTree and sends them for simulation
    fn send_new_tasks_for_simulation(&mut self) {
        // submit sim tasks loop
        loop {
            let mut new_sim_request = self.sim_tree.pop_simulation_tasks(1024);
            if new_sim_request.is_empty() {
                break;
            }
            // filter out cancelled orders
            new_sim_request.retain(|s| self.order_still_valid(&s.order.id()));

            for sim_request in new_sim_request {
                let order_id = sim_request.order.id();
                let delivered = match self.sim_req_sender.try_send(sim_request) {
                    Ok(()) => true,
                    Err(flume::TrySendError::Full(_)) => {
                        warn!("Sim channel is full, dropping order");
                        false
                        // @Metric
                    }
                    Err(flume::TrySendError::Disconnected(_)) => {
                        error!("Sim channel is closed, dropping order");
                        false
                        // @Metric
                    }
                };
                if !delivered {
                    // Small bug, if a cancel arrives we are going to propagate it.
                    self.in_flight_orders.remove(&order_id);
                }
            }
        }
    }

    /// updates the sim_tree and notifies new orders
    /// ONLY not cancelled are considered
    /// return if everything went OK
    async fn process_new_simulations(
        &mut self,
        new_sim_results: &mut Vec<SimulatedResult>,
    ) -> bool {
        // send results
        let mut valid_simulated_orders = Vec::new();
        for sim_result in new_sim_results {
            trace!(order_id=?sim_result.simulated_order.order.id(),
            sim_duration_mus = sim_result.simulation_time.as_micros(),
            profit = format_ether(sim_result.simulated_order.sim_value.coinbase_profit),
            "Order simulated");
            self.orders_simulated_ok
                .accumulate(&sim_result.simulated_order.order);
            // Skip cancelled orders and remove from in_flight_orders
            if self
                .in_flight_orders
                .remove(&sim_result.simulated_order.id())
            {
                valid_simulated_orders.push(sim_result.clone());
                if self
                    .slot_sim_results_sender
                    .send(SimulatedOrderCommand::Simulation(
                        sim_result.simulated_order.clone(),
                    ))
                    .await
                    .is_err()
                {
                    return false; //receiver closed :(
                }
            }
        }
        // update simtree
        if let Err(err) = self
            .sim_tree
            .submit_simulation_tasks_results(valid_simulated_orders)
        {
            error!(?err, "Failed to push order sim results into the sim tree");
            // @Metric
            return false;
        }
        true
    }

    /// return if everything went OK
    async fn send_cancel(&mut self, id: &OrderId) -> bool {
        self.slot_sim_results_sender
            .send(SimulatedOrderCommand::Cancellation(*id))
            .await
            .is_ok()
    }

    /// return if everything went OK
    async fn process_order_cancellation(&mut self, cancellation_id: &OrderId) -> bool {
        if !self.in_flight_orders.remove(cancellation_id) {
            // if we removed from in_flight_orders it was never sent so there is no need to cancel
            return self.send_cancel(cancellation_id).await;
        }
        true
    }

    /// feeding the sim tree.
    fn process_new_order(&mut self, order: Order) -> bool {
        self.orders_received.accumulate(&order);
        let order_id = order.id();
        if let Err(err) = self.sim_tree.push_orders(vec![order]) {
            error!(?err, "Failed to push order into the sim tree");
            // @Metric
            return false;
        }
        self.in_flight_orders.insert(order_id);
        true
    }

    async fn process_new_commands(&mut self, new_commands: &[OrderPoolCommand]) -> bool {
        for new_commnad in new_commands {
            match new_commnad {
                OrderPoolCommand::Insert(order) => {
                    if !self.process_new_order(order.clone()) {
                        return false;
                    }
                }
                OrderPoolCommand::Remove(order_id) => {
                    if !self.process_order_cancellation(order_id).await {
                        return false;
                    }
                }
            }
        }
        true
    }
}

// input new slot and a
impl<DB: Database + Clone + Send + 'static> OrderSimulationPool<DB> {
    pub fn new(
        provider_factory: ProviderFactoryReopener<DB>,
        num_workers: usize,
        global_cancellation: CancellationToken,
    ) -> Self {
        let mut result = Self {
            provider_factory,
            running_tasks: Arc::new(Mutex::new(Vec::new())),
            current_contexts: Arc::new(Mutex::new(CurrentSimulationContexts {
                contexts: HashMap::default(),
            })),
            worker_threads: Vec::new(),
        };
        for i in 0..num_workers {
            let ctx = Arc::clone(&result.current_contexts);
            let provider = result.provider_factory.clone();
            let cancel = global_cancellation.clone();
            let handle = std::thread::Builder::new()
                .name(format!("sim_thread:{}", i))
                .spawn(move || {
                    sim_worker::run_sim_worker(i, ctx, provider, cancel);
                })
                .expect("Failed to start sim worker thread");
            result.worker_threads.push(handle);
        }
        result
    }

    pub fn spawn_simulation_job(
        &self,
        ctx: BlockBuildingContext,
        input: OrdersForBlock,
        block_cancellation: CancellationToken,
    ) -> SlotOrderSimResults {
        let (slot_sim_results_sender, slot_sim_results_receiver) = mpsc::channel(10_000);

        let provider = self.provider_factory.provider_factory_unchecked();

        let current_contexts = Arc::clone(&self.current_contexts);
        let block_context: BlockContextId = gen_uid();
        let span = info_span!("sim_ctx", block = ctx.block_env.number.to::<u64>(), parent = ?ctx.attributes.parent);

        let handle = tokio::spawn(
            async move {
                debug!("Starting simulation job for parent block");
                let sim_tree = SimTree::new(provider, ctx.attributes.parent);
                let new_order_sub = input.new_order_sub;
                let (sim_req_sender, sim_req_receiver) = flume::unbounded();
                let (sim_results_sender, sim_results_receiver) = mpsc::channel(1024);
                {
                    let mut contexts = current_contexts.lock().unwrap();
                    let sim_context = SimulationContext {
                        block_ctx: ctx,
                        requests: sim_req_receiver,
                        results: sim_results_sender,
                    };
                    contexts.contexts.insert(block_context, sim_context);
                }
                let mut simulation_job = SimulationJob {
                    new_order_sub,
                    sim_req_sender,
                    sim_results_receiver,
                    slot_sim_results_sender,
                    sim_tree,
                    block_cancellation,
                    orders_received: OrderCounter::default(),
                    orders_simulated_ok: OrderCounter::default(),
                    in_flight_orders: Default::default(),
                };

                simulation_job.run().await;

                // clean up
                {
                    let mut contexts = current_contexts.lock().unwrap();
                    contexts.contexts.remove(&block_context);
                }
                info!(
                    ?simulation_job.orders_received,
                    ?simulation_job.orders_simulated_ok,
                    "Stopping simulation job "
                );
            }
            .instrument(span),
        );

        {
            let mut tasks = self.running_tasks.lock().unwrap();
            tasks.retain(|handle| !handle.is_finished());
            tasks.push(handle);
        }

        SlotOrderSimResults {
            orders: slot_sim_results_receiver,
        }
    }
}
