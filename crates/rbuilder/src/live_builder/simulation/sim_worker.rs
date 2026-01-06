use crate::{
    building::{
        sim::{DependencyKey, NonceKey, OrderSimResult, SimulatedResult},
        simulate_order, BlockState, ThreadBlockBuildingContext,
    },
    live_builder::simulation::CurrentSimulationContexts,
    provider::StateProviderFactory,
    telemetry::{self, add_sim_thread_utilisation_timings, mark_order_simulation_end},
};
use parking_lot::Mutex;
use rbuilder_primitives::ace::AceInteraction;
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
                task.order.clone(),
                &current_sim_context.block_ctx,
                &mut local_ctx,
                &mut block_state,
                &current_sim_context.ace_configs,
                &task.ace_unlock_contracts,
            );
            let sim_ok = match sim_result {
                Ok(sim_result) => {
                    let sim_ok = match sim_result.result {
                        OrderSimResult::Success(simulated_order, nonces_after) => {
                            let mut dependencies_satisfied: Vec<DependencyKey> = nonces_after
                                .into_iter()
                                .map(|(address, nonce)| {
                                    DependencyKey::Nonce(NonceKey { address, nonce })
                                })
                                .collect();

                            // If this is an unlocking ACE order, add the ACE dependency
                            if let Some(AceInteraction::Unlocking {
                                contract_address, ..
                            }) = simulated_order.ace_interaction
                            {
                                dependencies_satisfied
                                    .push(DependencyKey::AceUnlock(contract_address));
                            }

                            let result = SimulatedResult::Success {
                                id: task.id,
                                simulated_order,
                                previous_orders: task.parents,
                                dependencies_satisfied,
                                simulation_time: start_time.elapsed(),
                            };
                            if current_sim_context.results.try_send(result).is_err() {
                                error!(
                                    ?order_id,
                                    "Failed to send simulation result - channel full or closed"
                                );
                            }
                            true
                        }
                        OrderSimResult::Failed(failure) => {
                            // Only send to SimTree if there's an ACE dependency to handle
                            if failure.ace_dependency.is_some() {
                                let result = SimulatedResult::Failed {
                                    id: task.id,
                                    order: task.order,
                                    failure,
                                    ace_unlock_contracts: task.ace_unlock_contracts.clone(),
                                    simulation_time: start_time.elapsed(),
                                };
                                if current_sim_context.results.try_send(result).is_err() {
                                    error!(
                                        ?order_id,
                                        "Failed to send Failed result with ACE dependency"
                                    );
                                }
                            }
                            false
                        }
                    };
                    telemetry::inc_simulated_orders(sim_ok);
                    telemetry::inc_simulation_gas_used(sim_result.gas_used);
                    sim_ok
                }
                Err(err) => {
                    error!(?err, ?order_id, "Critical error while simulating order");
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
