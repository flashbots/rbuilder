pub mod sim_worker;
mod simulation_job;

use crate::{
    building::{
        sim::{SimTree, SimulatedResult, SimulationRequest},
        BlockBuildingContext,
    },
    live_builder::order_input::orderpool::OrdersForBlock,
    primitives::{OrderId, SimulatedOrder},
    utils::{gen_uid, ProviderFactoryReopener},
};
use ahash::HashMap;
use reth_db::database::Database;
use simulation_job::SimulationJob;
use std::sync::{Arc, Mutex};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{info_span, Instrument};

#[derive(Debug)]
pub struct SlotOrderSimResults {
    pub orders: mpsc::Receiver<SimulatedOrderCommand>,
}

type BlockContextId = u64;

/// Struct representing the need of order simulation for a particular block.
#[derive(Debug, Clone)]
pub struct SimulationContext {
    pub block_ctx: BlockBuildingContext,
    /// Simulation requests come in though this channel.
    pub requests: flume::Receiver<SimulationRequest>,
    /// Simulation results go out though this channel.
    pub results: mpsc::Sender<SimulatedResult>,
}

/// All active SimulationContexts
#[derive(Debug)]
pub struct CurrentSimulationContexts {
    pub contexts: HashMap<BlockContextId, SimulationContext>,
}

/// Struct that creates several [`sim_worker::run_sim_worker`] threads to allow concurrent simulation for the same block.
/// Usage:
/// 1 Create a single instance via [`OrderSimulationPool::new`] which receives the input.
/// 2 For each block call [`OrderSimulationPool::spawn_simulation_job`] which will spawn a task to run the simulations.
/// 3 Poll the results via the [`SlotOrderSimResults::orders`].
/// 4 IMPORTANT: When done with the simulations signal the provided block_cancellation.

#[derive(Debug)]
pub struct OrderSimulationPool<DB> {
    provider_factory: ProviderFactoryReopener<DB>,
    running_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    current_contexts: Arc<Mutex<CurrentSimulationContexts>>,
    worker_threads: Vec<std::thread::JoinHandle<()>>,
}

/// Result of a simulation.
#[derive(Clone, Debug)]
pub enum SimulatedOrderCommand {
    /// New simulation.
    Simulation(SimulatedOrder),
    /// Forwarded cancellation from the order source.
    Cancellation(OrderId),
}

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

    /// Prepares the context to run a SimulationJob and spawns a task with it.
    /// The returned SlotOrderSimResults can be polled to the the simulation stream.
    /// IMPORTANT: By calling spawn_simulation_job we lock some worker threads on the given block.
    ///     When we are done we MUST call block_cancellation so the threads can be freed for the next block.
    /// @Pending: Not properly working to be used with several blocks at the same time (forks!).
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
                let mut simulation_job = SimulationJob::new(
                    block_cancellation,
                    new_order_sub,
                    sim_req_sender,
                    sim_results_receiver,
                    slot_sim_results_sender,
                    sim_tree,
                );

                simulation_job.run().await;

                // clean up
                {
                    let mut contexts = current_contexts.lock().unwrap();
                    contexts.contexts.remove(&block_context);
                }
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
