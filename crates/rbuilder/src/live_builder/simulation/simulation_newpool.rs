use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;

use crate::{
    building::{
        sim::{NonceKey, OrderSimResult, SimTree, SimulatedResult},
        simulate_order, BlockBuildingContext, BlockState, ThreadBlockBuildingContext,
    },
    live_builder::order_input::orderpool,
    orderpool2::{NewOrderPool, NewOrderpoolUpdate, OrderScore},
    provider::StateProviderFactory,
    telemetry,
};
use orderpool_global_priority::block_pool::OrderpoolShard;

use super::simulation_job_tracer::SimulationJobTracer;
use tracing::error;

#[derive(Debug, Clone)]
pub struct NewOrderSimulationPool<P> {
    provider: P,
    workers: usize,
    use_random_coinbase: bool,
}

impl<P> NewOrderSimulationPool<P>
where
    P: StateProviderFactory + Clone + 'static,
{
    pub fn new(provider: P, num_workers: usize, use_random_coinbase: bool) -> Self {
        Self {
            provider,
            workers: num_workers,
            use_random_coinbase,
        }
    }

    pub fn spawn_simulation_job(
        &self,
        ctx: BlockBuildingContext,
        pool: NewOrderPool,
        block_cancellation: CancellationToken,
        sim_tracer: Arc<dyn SimulationJobTracer>,
    ) {
        for worker_id in 0..self.workers {
            let self_clone = self.clone();
            let ctx = ctx.clone();
            let pool = pool.clone();
            let block_cancellation = block_cancellation.clone();
            let sim_tracer = sim_tracer.clone();
	    let worker_count = self.workers;
            std::thread::Builder::new()
                .name(format!("newpool_sim_thread:{worker_id}"))
                .spawn(move || {
                    self_clone.run_simulation_thread(
                        ctx,
                        pool,
                        block_cancellation,
                        sim_tracer,
                        worker_id,
			worker_count,
                    )
                });
        }
    }

    fn run_simulation_thread(
        &self,
        ctx: BlockBuildingContext,
        pool: NewOrderPool,
        block_cancellation: CancellationToken,
        sim_tracer: Arc<dyn SimulationJobTracer>,
        worker_id: usize,
	worker_count: usize,
    ) {
        let from = OrderScore {
            is_simulated: false,
            high_priority: false,
            profit: Default::default(),
        };
        let subscription = pool.subscribe_with_shard(from.., Some(OrderpoolShard { shard_idx: worker_id as u64, shard_count: worker_count as u64}));

        let mut sim_tree = SimTree::new(pool.nonce_source.clone());

        let mut local_ctx = ThreadBlockBuildingContext::default();
        let mut last_sim_finished = Instant::now();
        let state_provider = match self.provider.history_by_block_hash(ctx.attributes.parent) {
            Ok(state_provider) => Arc::new(state_provider),
            Err(err) => {
                error!(?err, "Error while getting state for block");
                unreachable!();
            }
        };

        loop {
            if block_cancellation.is_cancelled() {
                return;
            }
            subscription.drop_updates();

            let order_id = match subscription.next_new_order() {
                Ok(Some(id)) => id,
                Ok(None) => {
                    // todo implement blocking next in new orderpool
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(_) => {
                    continue;
                }
            };
            let Some(order) = subscription.clone_order(&order_id) else {
                continue;
            };
            // TMP
            // tracing::warn!(?order_id, "New order for new sim");
            let id = order.id;
            sim_tree.push_orders(vec![order.order]).unwrap_or_default();
            let mut requests = sim_tree.pop_simulation_tasks(100);

            while let Some(task) = requests.pop() {
                let sim_thread_wait_time = last_sim_finished.elapsed();
                let sim_start = Instant::now();

                let order_id = task.order.id();
                let start_time = Instant::now();
                let mut block_state = BlockState::new_arc(state_provider.clone());
                let sim_result = simulate_order(
                    task.parents.clone(),
                    task.order,
                    &ctx,
                    &mut local_ctx,
                    &mut block_state,
                );
                let sim_ok = match sim_result {
                    Ok(sim_result) => {
                        let sim_ok = match sim_result.result {
                            OrderSimResult::Success(simulated_order, nonces_after) => {
                                pool.update_order(
                                    &simulated_order.id(),
                                    NewOrderpoolUpdate {
                                        sim_order: simulated_order.clone(),
                                    },
                                );
                                let result = SimulatedResult {
                                    id: task.id,
                                    simulated_order,
                                    previous_orders: task.parents,
                                    nonces_after: nonces_after
                                        .into_iter()
                                        .map(|(address, nonce)| NonceKey { address, nonce })
                                        .collect(),
                                    simulation_time: start_time.elapsed(),
                                };
                                sim_tree
                                    .submit_simulation_tasks_results(vec![result])
                                    .unwrap_or_default();
                                true
                            }
                            OrderSimResult::Failed(_) => false,
                        };
                        telemetry::inc_simulated_orders(sim_ok);
                        telemetry::inc_simulation_gas_used(sim_result.gas_used);
                        sim_ok
                    }
                    Err(err) => {
                        error!(?err, ?order_id, "Critical error while simulating order");
                        // @Metric
                        break;
                    }
                };

                telemetry::mark_order_simulation_end(order_id, sim_ok);
                last_sim_finished = Instant::now();
                let sim_thread_work_time = sim_start.elapsed();
                telemetry::add_sim_thread_utilisation_timings(
                    sim_thread_work_time,
                    sim_thread_wait_time,
                    worker_id,
                );
            }
        }
    }
}
