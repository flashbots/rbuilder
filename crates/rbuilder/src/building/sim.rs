use super::{
    create_sim_value,
    tracers::{AccumulatorSimulationTracer, SimulationTracer},
    OrderErr, PartialBlockFork, ThreadBlockBuildingContext,
};
use crate::{
    building::{
        BlockBuildingContext, BlockBuildingSpaceState, BlockState, CriticalCommitOrderError,
        NullPartialBlockForkExecutionTracer,
    },
    live_builder::order_input::mempool_txs_detector::MempoolTxsDetector,
    provider::StateProviderFactory,
    telemetry::{add_order_simulation_time, mark_order_pending_nonce},
    utils::NonceCache,
};
use ahash::{HashMap, HashSet};
use alloy_primitives::Address;
use alloy_primitives::U256;
use alloy_rpc_types::TransactionTrait;
use rand::seq::SliceRandom;
use rbuilder_primitives::ace::{
    classify_ace_interaction, AceInteraction, AceUnlockSource, Selector,
};
use rbuilder_primitives::AceConfig;
use rbuilder_primitives::BlockSpace;
use rbuilder_primitives::SimValue;
use rbuilder_primitives::{Order, OrderId, SimulatedOrder};
use reth_errors::ProviderError;
use reth_provider::StateProvider;
use std::{
    cmp::{max, min, Ordering},
    collections::hash_map::Entry,
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::{error, trace};

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum OrderSimResult {
    Success(Arc<SimulatedOrder>, Vec<(Address, u64)>),
    Failed(OrderErr),
}

#[derive(Debug)]
pub struct OrderSimResultWithGas {
    pub result: OrderSimResult,
    /// gas_used includes ANY gas consumed (eg: reverted txs)
    pub gas_used: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NonceKey {
    pub address: Address,
    pub nonce: u64,
}

/// Generic dependency key - represents something an order needs before it can execute
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DependencyKey {
    /// Order needs a specific nonce to be filled
    Nonce(NonceKey),
    /// Order needs an ACE unlock transaction for the given contract address
    AceUnlock(Address),
}

impl From<NonceKey> for DependencyKey {
    fn from(nonce: NonceKey) -> Self {
        DependencyKey::Nonce(nonce)
    }
}

/// State for a specific ACE exchange
#[derive(Debug, Clone, Default)]
pub struct AceExchangeState {
    /// Force ACE protocol order - always included
    pub force_unlock_order: Option<Arc<SimulatedOrder>>,
    /// Optional ACE protocol order - can be cancelled if mempool unlock arrives
    pub optional_unlock_order: Option<Arc<SimulatedOrder>>,
    /// Whether we've seen a mempool unlocking order (cancels optional)
    pub has_mempool_unlock: bool,
}

impl AceExchangeState {
    /// Get the best available unlock order.
    /// Selects the cheapest (lowest gas) for frontrunning when both are available.
    pub fn get_unlock_order(&self) -> Option<&Arc<SimulatedOrder>> {
        match (&self.force_unlock_order, &self.optional_unlock_order) {
            (Some(force), Some(optional)) => {
                // Select cheapest (lowest gas) for frontrunning
                if force.sim_value.gas_used() <= optional.sim_value.gas_used() {
                    Some(force)
                } else {
                    Some(optional)
                }
            }
            (Some(force), None) => Some(force),
            (None, Some(optional)) => Some(optional),
            (None, None) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingOrder {
    order: Order,
    unsatisfied_dependencies: usize,
}

pub type SimulationId = u64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SimulationRequest {
    pub id: SimulationId,
    pub order: Order,
    pub parents: Vec<Order>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedResult {
    pub id: SimulationId,
    pub simulated_order: Arc<SimulatedOrder>,
    pub previous_orders: Vec<Order>,
    /// Dependencies this simulation satisfies (nonces updated, ACE unlocks provided)
    pub dependencies_satisfied: Vec<DependencyKey>,
    pub simulation_time: Duration,
}

// @Feat replaceable orders
#[derive(Debug)]
pub struct SimTree {
    // fields for nonce management
    nonces: NonceCache,

    sims: HashMap<SimulationId, SimulatedResult>,
    /// Maps a dependency to the simulation that provides it (for single-dependency sims)
    dependency_providers: HashMap<DependencyKey, SimulationId>,

    pending_orders: HashMap<OrderId, PendingOrder>,

    /// Orders waiting on each dependency
    pending_dependencies: HashMap<DependencyKey, Vec<OrderId>>,

    ready_orders: Vec<SimulationRequest>,

    // ACE state management
    /// ACE configuration lookup by contract address
    ace_config: HashMap<Address, AceConfig>,
    /// ACE state (force/optional unlocks, mempool unlock tracking) by contract address
    ace_state: HashMap<Address, AceExchangeState>,
}

#[derive(Debug)]
enum OrderDependencyState {
    Invalid,
    Pending(Vec<DependencyKey>),
    Ready(Vec<Order>),
}

impl SimTree {
    pub fn new(nonce_cache_ref: NonceCache, ace_configs: Vec<AceConfig>) -> Self {
        let mut ace_config = HashMap::default();
        let mut ace_state = HashMap::default();

        for config in ace_configs {
            let contract_address = config.contract_address;
            ace_config.insert(contract_address, config);
            ace_state.insert(contract_address, AceExchangeState::default());
        }

        Self {
            nonces: nonce_cache_ref,
            sims: HashMap::default(),
            dependency_providers: HashMap::default(),
            pending_orders: HashMap::default(),
            pending_dependencies: HashMap::default(),
            ready_orders: Vec::default(),
            ace_config,
            ace_state,
        }
    }

    /// Get the ACE configs
    pub fn ace_configs(&self) -> &HashMap<Address, AceConfig> {
        &self.ace_config
    }

    /// Get the ACE state for a given contract address
    pub fn get_ace_state(&self, contract_address: &Address) -> Option<&AceExchangeState> {
        self.ace_state.get(contract_address)
    }

    fn push_order(&mut self, order: Order) -> Result<(), ProviderError> {
        if self.pending_orders.contains_key(&order.id()) {
            return Ok(());
        }

        let order_dep_state = self.get_order_dependency_state(&order)?;

        let order_id = order.id();

        match order_dep_state {
            OrderDependencyState::Invalid => {
                return Ok(());
            }
            OrderDependencyState::Pending(pending_deps) => {
                mark_order_pending_nonce(order_id);
                let unsatisfied_dependencies = pending_deps.len();
                for dep in pending_deps {
                    self.pending_dependencies
                        .entry(dep)
                        .or_default()
                        .push(order.id());
                }
                self.pending_orders.insert(
                    order.id(),
                    PendingOrder {
                        order,
                        unsatisfied_dependencies,
                    },
                );
            }
            OrderDependencyState::Ready(parents) => {
                self.ready_orders.push(SimulationRequest {
                    id: rand::random(),
                    order,
                    parents,
                });
            }
        }
        Ok(())
    }

    fn get_order_dependency_state(
        &mut self,
        order: &Order,
    ) -> Result<OrderDependencyState, ProviderError> {
        let mut onchain_nonces_incremented = HashSet::default();
        let mut pending_deps = Vec::new();
        let mut parent_orders = Vec::new();

        // Check nonce dependencies
        for nonce in order.nonces() {
            let onchain_nonce = self.nonces.nonce(nonce.address)?;

            match onchain_nonce.cmp(&nonce.nonce) {
                Ordering::Equal => {
                    // nonce valid
                    onchain_nonces_incremented.insert(nonce.address);
                    continue;
                }
                Ordering::Greater => {
                    // nonce invalid, maybe its optional
                    if !nonce.optional {
                        // this order will never be valid
                        trace!(
                            order = ?order.id(),
                            ?nonce,
                            "Dropping order because of nonce"
                        );
                        return Ok(OrderDependencyState::Invalid);
                    } else {
                        // we can ignore this tx
                        continue;
                    }
                }
                Ordering::Less => {
                    if onchain_nonces_incremented.contains(&nonce.address) {
                        // we already considered this account nonce
                        continue;
                    }
                    // mark this nonce as considered
                    onchain_nonces_incremented.insert(nonce.address);

                    let nonce_key = NonceKey {
                        address: nonce.address,
                        nonce: nonce.nonce,
                    };
                    let dep_key = DependencyKey::Nonce(nonce_key);

                    if let Some(sim_id) = self.dependency_providers.get(&dep_key) {
                        // we have something that fills this nonce
                        let sim = self.sims.get(sim_id).expect("we never delete sims");
                        parent_orders.extend_from_slice(&sim.previous_orders);
                        parent_orders.push(sim.simulated_order.order.clone());
                        continue;
                    }

                    pending_deps.push(dep_key);
                }
            }
        }

        if pending_deps.is_empty() {
            Ok(OrderDependencyState::Ready(parent_orders))
        } else {
            Ok(OrderDependencyState::Pending(pending_deps))
        }
    }

    /// Check if an order needs ACE unlock and add that dependency.
    /// Called after initial simulation when we detect a NonUnlocking ACE interaction.
    fn add_ace_dependency_for_order(
        &mut self,
        order: Order,
        contract_address: Address,
    ) -> Result<(), ProviderError> {
        let dep_key = DependencyKey::AceUnlock(contract_address);

        // Check if we already have an unlock provider
        if let Some(sim_id) = self.dependency_providers.get(&dep_key) {
            let sim = self.sims.get(sim_id).expect("we never delete sims");
            let mut parents = sim.previous_orders.clone();
            parents.push(sim.simulated_order.order.clone());

            // Order is ready with the unlock tx as parent
            self.ready_orders.push(SimulationRequest {
                id: rand::random(),
                order,
                parents,
            });
        } else {
            // No unlock yet - add to pending
            self.pending_dependencies
                .entry(dep_key)
                .or_default()
                .push(order.id());
            self.pending_orders.insert(
                order.id(),
                PendingOrder {
                    order,
                    unsatisfied_dependencies: 1,
                },
            );
        }
        Ok(())
    }

    pub fn push_orders(&mut self, orders: Vec<Order>) -> Result<(), ProviderError> {
        for order in orders {
            self.push_order(order)?;
        }
        Ok(())
    }

    pub fn pop_simulation_tasks(&mut self, limit: usize) -> Vec<SimulationRequest> {
        let limit = min(limit, self.ready_orders.len());
        self.ready_orders.drain(..limit).collect()
    }

    // we don't really need state here because nonces are cached but its smaller if we reuse pending state fn
    fn process_simulation_task_result(
        &mut self,
        result: SimulatedResult,
    ) -> Result<(), ProviderError> {
        self.sims.insert(result.id, result.clone());
        let mut orders_ready = Vec::new();

        // Process each dependency this simulation satisfies
        if result.dependencies_satisfied.len() == 1 {
            let dep_key = result
                .dependencies_satisfied
                .first()
                .expect("checked len == 1")
                .clone();

            match self.dependency_providers.entry(dep_key.clone()) {
                Entry::Occupied(mut entry) => {
                    // Already have a provider - check if this one is more profitable
                    let current_sim_profit = {
                        let sim_id = entry.get_mut();
                        self.sims
                            .get(sim_id)
                            .expect("we never delete sims")
                            .simulated_order
                            .sim_value
                            .full_profit_info()
                            .coinbase_profit()
                    };
                    if result
                        .simulated_order
                        .sim_value
                        .full_profit_info()
                        .coinbase_profit()
                        > current_sim_profit
                    {
                        entry.insert(result.id);
                    }
                }
                Entry::Vacant(entry) => {
                    // First provider for this dependency
                    entry.insert(result.id);

                    // Unblock orders waiting on this dependency
                    if let Some(pending_order_ids) = self.pending_dependencies.remove(&dep_key) {
                        for order_id in pending_order_ids {
                            match self.pending_orders.entry(order_id) {
                                Entry::Occupied(mut entry) => {
                                    let pending_order = entry.get_mut();
                                    pending_order.unsatisfied_dependencies -= 1;
                                    if pending_order.unsatisfied_dependencies == 0 {
                                        orders_ready.push(entry.remove().order);
                                    }
                                }
                                Entry::Vacant(_) => {
                                    error!("SimTree bug order not found");
                                    // @Metric bug counter
                                }
                            }
                        }
                    }
                }
            }
        }

        for ready_order in orders_ready {
            let pending_state = self.get_order_dependency_state(&ready_order)?;
            match pending_state {
                OrderDependencyState::Ready(parents) => {
                    self.ready_orders.push(SimulationRequest {
                        id: rand::random(),
                        order: ready_order,
                        parents,
                    });
                }
                OrderDependencyState::Invalid => {
                    // @Metric bug counter
                    error!("SimTree bug order became invalid");
                }
                OrderDependencyState::Pending(_) => {
                    // @Metric bug counter
                    error!("SimTree bug order became pending again");
                }
            }
        }
        Ok(())
    }

    /// Handle ACE interaction after simulation.
    /// Returns (was_handled, optional_cancellation_order_id)
    /// - For Unlocking interactions: registers as force or optional unlock provider
    /// - For NonUnlocking interactions: adds order as pending on ACE unlock dependency
    /// - Returns cancellation OrderId if a mempool unlock cancels an optional ACE tx
    pub fn handle_ace_interaction(
        &mut self,
        result: &mut SimulatedResult,
    ) -> Result<(bool, Option<OrderId>), ProviderError> {
        let Some(interaction) = result.simulated_order.ace_interaction else {
            return Ok((false, None));
        };

        // If this order already has parents, it was re-simulated with unlock - just pass through
        if !result.previous_orders.is_empty() {
            return Ok((false, None));
        }

        let mut cancellation = None;

        match interaction {
            AceInteraction::Unlocking {
                contract_address,
                source,
            } => {
                // Register the unlock in ACE state
                let state = self.ace_state.entry(contract_address).or_default();

                if source == AceUnlockSource::ProtocolForce {
                    state.force_unlock_order = Some(result.simulated_order.clone());
                    trace!(
                        "Added forced ACE protocol unlock order for {:?}",
                        contract_address
                    );
                } else {
                    state.optional_unlock_order = Some(result.simulated_order.clone());
                    trace!(
                        "Added optional ACE protocol unlock order for {:?}",
                        contract_address
                    );
                }

                // Check if we should cancel the optional ACE order (mempool unlock arrived first)
                if state.has_mempool_unlock {
                    if let Some(optional) = state.optional_unlock_order.take() {
                        cancellation = Some(optional.order.id());
                    }
                }

                // Make sure the ACE unlock dependency is in dependencies_satisfied
                let dep_key = DependencyKey::AceUnlock(contract_address);
                if !result.dependencies_satisfied.contains(&dep_key) {
                    result.dependencies_satisfied.push(dep_key);
                }

                // Process this result to unblock pending orders
                self.process_simulation_task_result(result.clone())?;
            }
            AceInteraction::NonUnlocking { contract_address } => {
                // This is a mempool order that needs ACE unlock
                let state = self.ace_state.entry(contract_address).or_default();

                // Check if we have an unlock order to use as parent
                if let Some(unlock_order) = state.get_unlock_order().cloned() {
                    // Re-queue with the unlock as parent
                    self.ready_orders.push(SimulationRequest {
                        id: rand::random(),
                        order: result.simulated_order.order.clone(),
                        parents: vec![unlock_order.order.clone()],
                    });
                } else {
                    // No unlock yet - add as pending on ACE dependency
                    self.add_ace_dependency_for_order(
                        result.simulated_order.order.clone(),
                        contract_address,
                    )?;
                }
                return Ok((true, None));
            }
        }

        Ok((true, cancellation))
    }

    /// Mark that a mempool unlocking order has been seen for a contract address.
    /// Returns the OrderId of the optional ACE order to cancel, if any.
    pub fn mark_mempool_unlock(&mut self, contract_address: Address) -> Option<OrderId> {
        let state = self.ace_state.entry(contract_address).or_default();

        // Only cancel once
        if state.has_mempool_unlock {
            return None;
        }
        state.has_mempool_unlock = true;

        // Cancel the optional ACE order if present
        state
            .optional_unlock_order
            .take()
            .map(|order| order.order.id())
    }

    pub fn submit_simulation_tasks_results(
        &mut self,
        results: Vec<SimulatedResult>,
    ) -> Result<(), ProviderError> {
        for result in results {
            self.process_simulation_task_result(result)?;
        }
        Ok(())
    }
}

/// Non-interactive usage of sim tree that will simply simulate all orders.
/// `randomize_insertion` is used to debug if sim tree works correctly when orders are inserted in a different order
/// outputs should be independent of this arg.
pub fn simulate_all_orders_with_sim_tree<P>(
    provider: P,
    ctx: &BlockBuildingContext,
    orders: &[Order],
    randomize_insertion: bool,
    ace_config: Vec<AceConfig>,
) -> Result<(Vec<Arc<SimulatedOrder>>, Vec<OrderErr>), CriticalCommitOrderError>
where
    P: StateProviderFactory + Clone,
{
    let nonces = {
        let state = provider.history_by_block_hash(ctx.attributes.parent)?;
        NonceCache::new(state.into())
    };
    let mut sim_tree = SimTree::new(nonces, ace_config);

    let mut orders = orders.to_vec();
    let random_insert_size = max(orders.len() / 20, 1);
    if randomize_insertion {
        let mut rng = rand::thread_rng();
        // shuffle orders
        orders.shuffle(&mut rng);
    } else {
        sim_tree.push_orders(orders.clone())?;
    }

    let mut sim_errors = Vec::new();
    let mut state_for_sim =
        Arc::<dyn StateProvider>::from(provider.history_by_block_hash(ctx.attributes.parent)?);
    let mut local_ctx = ThreadBlockBuildingContext::default();
    loop {
        // mix new orders into the sim_tree
        if randomize_insertion && !orders.is_empty() {
            let insert_size = min(random_insert_size, orders.len());
            let orders = orders.drain(..insert_size).collect::<Vec<_>>();
            sim_tree.push_orders(orders)?;
        }

        let sim_tasks = sim_tree.pop_simulation_tasks(1000);
        if sim_tasks.is_empty() {
            if randomize_insertion && !orders.is_empty() {
                continue;
            } else {
                break;
            }
        }

        let mut sim_results = Vec::new();
        for sim_task in sim_tasks {
            let start_time = Instant::now();
            let mut block_state = BlockState::new_arc(state_for_sim);
            let sim_result = simulate_order(
                sim_task.parents.clone(),
                sim_task.order.clone(),
                ctx,
                &mut local_ctx,
                &mut block_state,
                sim_tree.ace_configs(),
            )?;
            let (_, provider) = block_state.into_parts();
            state_for_sim = provider;
            match sim_result.result {
                OrderSimResult::Failed(err) => {
                    trace!(
                        order = sim_task.order.id().to_string(),
                        ?err,
                        "Order simulation failed"
                    );
                    sim_errors.push(err);
                    continue;
                }
                OrderSimResult::Success(sim_order, nonces) => {
                    let mut dependencies_satisfied: Vec<DependencyKey> = nonces
                        .into_iter()
                        .map(|(address, nonce)| DependencyKey::Nonce(NonceKey { address, nonce }))
                        .collect();

                    // If this is an unlocking ACE order, add the ACE dependency
                    if let Some(AceInteraction::Unlocking {
                        contract_address, ..
                    }) = sim_order.ace_interaction
                    {
                        dependencies_satisfied.push(DependencyKey::AceUnlock(contract_address));
                    }

                    let result = SimulatedResult {
                        id: sim_task.id,
                        simulated_order: sim_order,
                        previous_orders: sim_task.parents,
                        dependencies_satisfied,
                        simulation_time: start_time.elapsed(),
                    };
                    sim_results.push(result);
                }
            }
        }
        sim_tree.submit_simulation_tasks_results(sim_results)?;
    }

    Ok((
        sim_tree
            .sims
            .into_values()
            .map(|sim| sim.simulated_order)
            .collect(),
        sim_errors,
    ))
}

/// Prepares context (fork + tracer) and calls simulate_order_using_fork
pub fn simulate_order(
    parent_orders: Vec<Order>,
    order: Order,
    ctx: &BlockBuildingContext,
    local_ctx: &mut ThreadBlockBuildingContext,
    state: &mut BlockState,
    ace_configs: &HashMap<Address, AceConfig>,
) -> Result<OrderSimResultWithGas, CriticalCommitOrderError> {
    let mut tracer = AccumulatorSimulationTracer::new();
    let mut fork = PartialBlockFork::new(state, ctx, local_ctx).with_tracer(&mut tracer);
    let rollback_point = fork.rollback_point();
    let sim_res = simulate_order_using_fork(
        parent_orders,
        order,
        &mut fork,
        &ctx.mempool_tx_detector,
        ace_configs,
    );
    fork.rollback(rollback_point);
    let sim_res = sim_res?;

    Ok(OrderSimResultWithGas {
        result: sim_res,
        gas_used: tracer.used_gas,
    })
}

/// Simulates order (including parent (those needed to reach proper nonces) orders) using a precreated fork
pub fn simulate_order_using_fork<Tracer: SimulationTracer>(
    parent_orders: Vec<Order>,
    order: Order,
    fork: &mut PartialBlockFork<'_, '_, '_, '_, Tracer, NullPartialBlockForkExecutionTracer>,
    mempool_tx_detector: &MempoolTxsDetector,
    ace_configs: &HashMap<Address, AceConfig>,
) -> Result<OrderSimResult, CriticalCommitOrderError> {
    let start = Instant::now();
    let has_parents = !parent_orders.is_empty();

    // simulate parents
    let mut space_state = BlockBuildingSpaceState::ZERO;
    // We use empty combined refunds because the value of the bundle will
    // not change from batching.
    let combined_refunds = std::collections::HashMap::default();
    for parent in parent_orders {
        let result = fork.commit_order(&parent, space_state, true, &combined_refunds)?;
        match result {
            Ok(res) => {
                space_state.use_space(res.space_used);
            }
            Err(err) => {
                tracing::trace!(parent_order = ?parent.id(), ?err, "failed to simulate parent order");
                return Ok(OrderSimResult::Failed(err));
            }
        }
    }

    // simulate
    let result = fork.commit_order(&order, space_state, true, &combined_refunds)?;
    let sim_time = start.elapsed();
    let sim_success = result.is_ok();
    add_order_simulation_time(sim_time, "sim", sim_success); // we count parent sim time + order sim time time here

    // Get the used_state_trace from tracer (available regardless of success/failure)
    let used_state_trace = fork
        .tracer
        .as_ref()
        .and_then(|t| t.get_used_state_tracer())
        .cloned();

    // Detect ACE interaction from the state trace using config
    // Get function selector and tx.to from order's first transaction
    let (selector, tx_to): (Option<Selector>, Option<Address>) =
        order.list_txs().first().map_or((None, None), |(tx, _)| {
            let input = tx.internal_tx_unsecure().input();
            let sel = if input.len() >= 4 {
                Some(Selector::from_slice(&input[..4]))
            } else {
                None
            };
            (sel, tx.to())
        });

    let ace_interaction = used_state_trace.as_ref().and_then(|trace| {
        ace_configs.iter().find_map(|(_, config)| {
            if !config.enabled {
                return None;
            }
            classify_ace_interaction(trace, sim_success, config, selector, tx_to)
        })
    });

    match result {
        Ok(res) => {
            let sim_value = create_sim_value(&order, &res, mempool_tx_detector);
            let new_nonces = res.nonces_updated.into_iter().collect::<Vec<_>>();
            Ok(OrderSimResult::Success(
                Arc::new(SimulatedOrder {
                    order,
                    sim_value,
                    used_state_trace: res.used_state_trace,
                    ace_interaction,
                }),
                new_nonces,
            ))
        }
        Err(err) => {
            // Check if failed order accessed ACE - if so, treat as successful with zero profit
            if let Some(interaction @ AceInteraction::NonUnlocking { contract_address }) =
                ace_interaction
            {
                // ACE can inject parent orders, we want to ignore these.
                if !has_parents {
                    tracing::debug!(
                        order = ?order.id(),
                        ?err,
                        ?contract_address,
                        "Failed order accessed ACE - treating as successful non-unlocking ACE order"
                    );
                    // For failed-but-ACE orders, we use 0 gas since the order
                    // didn't actually succeed - it's just marked as a non-unlocking ACE interaction
                    let gas_used = 0;
                    return Ok(OrderSimResult::Success(
                        Arc::new(SimulatedOrder {
                            order,
                            sim_value: SimValue::new(
                                U256::ZERO,
                                U256::ZERO,
                                BlockSpace::new(gas_used, 0, 0),
                                Vec::new(),
                            ),
                            used_state_trace,
                            ace_interaction: Some(interaction),
                        }),
                        Vec::new(),
                    ));
                }
            }
            Ok(OrderSimResult::Failed(err))
        }
    }
}
