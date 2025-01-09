use crate::{
    building::{
        sim::{NonceKey, OrderSimResult, SimulatedResult},
        simulate_order, BlockState,
    },
    live_builder::simulation::CurrentSimulationContexts,
    provider::StateProviderFactory,
    telemetry,
    telemetry::add_sim_thread_utilisation_timings,
};
use parking_lot::Mutex;
use reth::revm::cached::CachedReads;
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
    loop {
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

        let mut cached_reads = CachedReads::default();
        let mut last_sim_finished = Instant::now();
        while let Ok(task) = current_sim_context.requests.recv() {
            let sim_thread_wait_time = last_sim_finished.elapsed();
            let sim_start = Instant::now();

            let state_provider = match provider
                .history_by_block_hash(current_sim_context.block_ctx.attributes.parent)
            {
                Ok(state_provider) => state_provider,
                Err(err) => {
                    error!(?err, "Error while getting state for block");
                    // break here so we can try to get new context
                    // @Metric
                    break;
                }
            };
            let start_time = Instant::now();
            let mut block_state = BlockState::new(state_provider).with_cached_reads(cached_reads);
            let sim_result = simulate_order(
                task.parents.clone(),
                task.order,
                &current_sim_context.block_ctx,
                &mut block_state,
            );
            match sim_result {
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
                }
                Err(err) => {
                    error!(?err, "Critical error while simulating order");
                    // @Metric
                    break;
                }
            }
            (cached_reads, _, _) = block_state.into_parts();

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
