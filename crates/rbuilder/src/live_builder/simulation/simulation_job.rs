use std::fmt;

use crate::{
    building::sim::{SimTree, SimulatedResult, SimulationRequest},
    live_builder::order_input::order_sink::OrderPoolCommand,
    primitives::{Order, OrderId},
};
use ahash::HashSet;
use alloy_primitives::utils::format_ether;
use reth_db::database::Database;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

use super::SimulatedOrderCommand;

/// Struct that continuously simulates orders.
/// Create and call run()
/// The flow is:
/// 1 New orders are polled from new_order_sub and inserted en the SimTree.
/// 2 SimTree is polled for nonce-ready orders and are sent to be simulated (sent to sim_req_sender).
/// 3 Simulation results are polled from sim_results_receiver and sent to slot_sim_results_sender.
/// Cancellation flow: we add every order we start to process to in_flight_orders.
/// If we get a cancellation and the order is not in in_flight_orders we forward the cancellation.
/// If we get a cancellation and the order is in in_flight_orders we just remove it from in_flight_orders.
/// Only SimulatedOrders still in in_flight_orders are delivered.
/// @Pending: implement cancellations in the SimTree.
pub struct SimulationJob<DB> {
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

    orders_received: OrderCounter,
    orders_simulated_ok: OrderCounter,

    /// Orders we got via new_order_sub and are still being processed (they could be inside the SimTree or in the sim queue)
    /// and were not cancelled.
    in_flight_orders: HashSet<OrderId>,
}

impl<DB: Database + Clone + Send + 'static> SimulationJob<DB> {
    pub fn new(
        block_cancellation: CancellationToken,
        new_order_sub: mpsc::UnboundedReceiver<OrderPoolCommand>,
        sim_req_sender: flume::Sender<SimulationRequest>,
        sim_results_receiver: mpsc::Receiver<SimulatedResult>,
        slot_sim_results_sender: mpsc::Sender<SimulatedOrderCommand>,
        sim_tree: SimTree<DB>,
    ) -> Self {
        Self {
            block_cancellation,
            new_order_sub,
            sim_req_sender,
            sim_results_receiver,
            slot_sim_results_sender,
            sim_tree,
            orders_received: OrderCounter::default(),
            orders_simulated_ok: OrderCounter::default(),
            in_flight_orders: Default::default(),
        }
    }

    pub async fn run(&mut self) {
        debug!("Starting simulation job for parent block");
        self.run_no_trace().await;
        info!(
            ?self.orders_received,
            ?self.orders_simulated_ok,
            "Stopping simulation job "
        );
    }
    async fn run_no_trace(&mut self) {
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

/// Helper to accumulate order count and trace it.
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
