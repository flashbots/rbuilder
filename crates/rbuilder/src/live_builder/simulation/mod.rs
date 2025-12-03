pub mod sim_worker;
mod simulation_job;
pub mod simulation_job_tracer;

use crate::{
    building::{
        sim::{SimTree, SimulatedResult, SimulationRequest},
        tx_sim_cache::TxExecutionCache,
        BlockBuildingContext,
    },
    live_builder::{
        order_input::orderpool::OrdersForBlock,
        simulation::simulation_job_tracer::SimulationJobTracer,
    },
    provider::StateProviderFactory,
    utils::{gen_uid, NonceCache, Signer},
};
use ahash::HashMap;
use parking_lot::Mutex;
use rbuilder_primitives::{OrderId, SimulatedOrder};
use simulation_job::SimulationJob;
use std::sync::Arc;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{error, info_span, Instrument};

#[derive(Debug)]
pub struct SlotOrderSimResults {
    pub orders: mpsc::Receiver<SimulatedOrderCommand>,
}

type BlockContextId = u64;

/// Struct representing the need of order simulation for a particular block.
#[derive(Debug, Clone)]
pub struct SimulationContext {
    pub block_ctx: BlockBuildingContext,
    /// Simulation requests come in through this channel.
    pub requests: flume::Receiver<SimulationRequest>,
    /// Simulation results go out through this channel.
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
pub struct OrderSimulationPool<P> {
    provider: P,
    running_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    current_contexts: Arc<Mutex<CurrentSimulationContexts>>,
    worker_threads: Vec<std::thread::JoinHandle<()>>,
    use_random_coinbase: bool,
}

/// Result of a simulation.
#[derive(Clone, Debug)]
pub enum SimulatedOrderCommand {
    /// New simulation.
    Simulation(Arc<SimulatedOrder>),
    /// Forwarded cancellation from the order source.
    Cancellation(OrderId),
}

impl<P> OrderSimulationPool<P>
where
    P: StateProviderFactory + Clone + 'static,
{
    pub fn new(
        provider: P,
        num_workers: usize,
        use_random_coinbase: bool,
        global_cancellation: CancellationToken,
    ) -> Self {
        let mut result = Self {
            provider,
            running_tasks: Arc::new(Mutex::new(Vec::new())),
            current_contexts: Arc::new(Mutex::new(CurrentSimulationContexts {
                contexts: HashMap::default(),
            })),
            worker_threads: Vec::new(),
            use_random_coinbase,
        };
        for i in 0..num_workers {
            let ctx = Arc::clone(&result.current_contexts);
            let provider = result.provider.clone();
            let cancel = global_cancellation.clone();
            let handle = std::thread::Builder::new()
                .name(format!("sim_thread:{i}"))
                .spawn(move || {
                    sim_worker::run_sim_worker(i, ctx, provider, cancel);
                })
                .expect("Failed to start sim worker thread");
            result.worker_threads.push(handle);
        }
        result
    }

    /// Prepares the context to run a SimulationJob and spawns a task with it.
    /// The returned SlotOrderSimResults can be polled to the simulation stream.
    /// IMPORTANT: By calling spawn_simulation_job we lock some worker threads on the given block.
    ///     When we are done we MUST call block_cancellation so the threads can be freed for the next block.
    /// @Pending: Not properly working to be used with several blocks at the same time (forks!).
    pub fn spawn_simulation_job(
        &self,
        ctx: BlockBuildingContext,
        input: OrdersForBlock,
        block_cancellation: CancellationToken,
        sim_tracer: Arc<dyn SimulationJobTracer>,
    ) -> SlotOrderSimResults {
        let (slot_sim_results_sender, slot_sim_results_receiver) = mpsc::channel(10_000);

        let ctx = if self.use_random_coinbase {
            // use random coinbase for simulations to make top of the block simulation bypass harder
            let mut ctx = ctx;
            let signer = Signer::random();
            ctx.evm_env.block_env.beneficiary = signer.address;
            ctx.builder_signer = signer;
            ctx.tx_execution_cache = TxExecutionCache::new(false).into();
            ctx
        } else {
            ctx
        };

        let provider = self.provider.clone();
        let current_contexts = Arc::clone(&self.current_contexts);
        let block_context: BlockContextId = gen_uid();
        let span = info_span!("sim_ctx", block = ctx.block(), parent = ?ctx.attributes.parent);

        let handle = tokio::spawn(
            async move {
                let nonces = {
                    let state = match provider.history_by_block_hash(ctx.attributes.parent) {
                        Ok(state) => state,
                        Err(err) => {
                            error!(
                                ?err,
                                "Failed to get history_by_block_hash, cancelling simulation job"
                            );
                            return;
                        }
                    };
                    NonceCache::new(state.into())
                };

                let sim_tree = SimTree::new(nonces);
                let new_order_sub = input.new_order_sub;
                let (sim_req_sender, sim_req_receiver) = flume::unbounded();
                let (sim_results_sender, sim_results_receiver) = mpsc::channel(1024);
                {
                    let mut contexts = current_contexts.lock();
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
                    sim_tracer,
                );

                simulation_job.run().await;

                // clean up
                {
                    let mut contexts = current_contexts.lock();
                    contexts.contexts.remove(&block_context);
                }
            }
            .instrument(span),
        );

        {
            let mut tasks = self.running_tasks.lock();
            tasks.retain(|handle| !handle.is_finished());
            tasks.push(handle);
        }

        SlotOrderSimResults {
            orders: slot_sim_results_receiver,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        building::testing::test_chain_state::{BlockArgs, NamedAddr, TestChainState, TxArgs},
        live_builder::{
            order_input::order_sink::OrderPoolCommand,
            simulation::simulation_job_tracer::NullSimulationJobTracer,
        },
        utils::ProviderFactoryReopener,
    };
    use alloy_primitives::U256;
    use rbuilder_primitives::{MempoolTx, Order, TransactionSignedEcRecoveredWithBlobs};

    #[tokio::test]
    async fn test_simulate_order_to_coinbase() {
        let test_context = TestChainState::new(BlockArgs::default().number(11)).unwrap();

        // Create simulation core
        let cancel = CancellationToken::new();
        let provider_factory_reopener = ProviderFactoryReopener::new_from_existing(
            test_context.provider_factory().clone(),
            None,
        )
        .unwrap();

        let sim_pool = OrderSimulationPool::new(provider_factory_reopener, 4, true, cancel.clone());
        let (order_sender, order_receiver) = mpsc::unbounded_channel();
        let orders_for_block = OrdersForBlock {
            new_order_sub: order_receiver,
        };

        let mut sim_results = sim_pool.spawn_simulation_job(
            test_context.block_building_context().clone(),
            orders_for_block,
            cancel.clone(),
            Arc::new(NullSimulationJobTracer {}),
        );
        // Create a simple tx that sends to coinbase 5 wei.
        let coinbase_profit = 5;
        // max_priority_fee will be 0
        let tx_args = TxArgs::new_send_to_coinbase(NamedAddr::User(1), 0, coinbase_profit);
        let tx = test_context.sign_tx(tx_args).unwrap();
        let tx = TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx).unwrap();
        order_sender
            .send(OrderPoolCommand::Insert(Order::Tx(MempoolTx::new(tx))))
            .unwrap();

        // We expect to receive the simulation giving a profit of coinbase_profit since that's what we sent directly to coinbase.
        // and we are not paying any priority fee
        if let Some(command) = sim_results.orders.recv().await {
            match command {
                SimulatedOrderCommand::Simulation(sim_order) => {
                    assert_eq!(
                        sim_order.sim_value.full_profit_info().coinbase_profit(),
                        U256::from(coinbase_profit)
                    );
                }
                SimulatedOrderCommand::Cancellation(_) => panic!("Cancellation not expected"),
            };
        }
    }
}
