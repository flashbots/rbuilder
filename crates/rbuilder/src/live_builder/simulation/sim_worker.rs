use crate::{
    building::{
        sim::{NonceKey, OrderSimResult, SimulatedResult},
        simulate_order, BlockState, ThreadBlockBuildingContext,
    },
    live_builder::simulation::CurrentSimulationContexts,
    provider::StateProviderFactory,
    telemetry::{self, add_sim_thread_utilisation_timings, mark_order_simulation_end},
};
use parking_lot::Mutex;
use std::{
    sync::Arc,
    thread::sleep,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::error;

/// Function that continuously looks for a SimulationContext on ctx and when it finds one it polls its "request for simulation" channel (SimulationContext::requests).
/// When the channel closes it goes back to waiting for a new SimulationContext.
/// It's blocking so it's expected to run in its own thread.
pub fn run_sim_worker<P>(
    worker_id: usize,
    ctx: Arc<Mutex<CurrentSimulationContexts>>,
    provider: P,
    global_cancellation: CancellationToken,
) where
    P: StateProviderFactory,
{
    'main: loop {
        if global_cancellation.is_cancelled() {
            return;
        }
        let current_sim_context = loop {
            let next_ctx = {
                let ctxs = ctx.lock();
                ctxs.contexts.iter().next().map(|(_, c)| c.clone())
            };
            // @Perf chose random context so its more fair when we have 2 instead of 1
            if let Some(ctx) = next_ctx {
                break ctx;
            } else {
                // contexts are created for a duration of the slot so this is not a problem
                sleep(Duration::from_millis(50));
            }
        };

        let mut local_ctx = ThreadBlockBuildingContext::default();

        let mut last_sim_finished = Instant::now();

        let state_provider =
            match provider.history_by_block_hash(current_sim_context.block_ctx.attributes.parent) {
                Ok(state_provider) => Arc::new(state_provider),
                Err(err) => {
                    error!(?err, "Error while getting state for block");
                    continue 'main;
                }
            };
        while let Ok(task) = current_sim_context.requests.recv() {
            let sim_thread_wait_time = last_sim_finished.elapsed();
            let sim_start = Instant::now();

            let order_id = task.order.id();
            let start_time = Instant::now();
            let mut block_state = BlockState::new_arc(state_provider.clone());
            let sim_result = simulate_order(
                task.parents.clone(),
                task.order,
                &current_sim_context.block_ctx,
                &mut local_ctx,
                &mut block_state,
            );
            let sim_ok = match sim_result {
                Ok(sim_result) => {
                    let sim_ok = match sim_result.result {
                        OrderSimResult::Success(simulated_order, nonces_after) => {
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
                            current_sim_context
                                .results
                                .try_send(result)
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

            mark_order_simulation_end(order_id, sim_ok);
            last_sim_finished = Instant::now();
            let sim_thread_work_time = sim_start.elapsed();
            add_sim_thread_utilisation_timings(
                sim_thread_work_time,
                sim_thread_wait_time,
                worker_id,
            );
        }
    }
}
