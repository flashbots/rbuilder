use super::{
    create_sim_value,
    tracers::{AccumulatorSimulationTracer, SimulationTracer},
    OrderErr, PartialBlockFork, ThreadBlockBuildingContext,
};
use crate::{
    building::{
        order_is_worth_executing, BlockBuildingContext, BlockBuildingSpaceState, BlockState,
        CriticalCommitOrderError, NullPartialBlockForkExecutionTracer,
    },
    live_builder::order_input::mempool_txs_detector::MempoolTxsDetector,
    provider::StateProviderFactory,
    telemetry::{add_order_simulation_time, mark_order_pending_nonce},
    utils::NonceCache,
};
use ahash::{HashMap, HashSet};
use alloy_primitives::Address;
use alloy_rpc_types::TransactionTrait;
use rand::seq::SliceRandom;
use rbuilder_primitives::ace::{
    classify_ace_interaction, AceInteraction, AceUnlockSource, Selector,
};
use rbuilder_primitives::AceConfig;
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

/// Information about a simulation failure
#[derive(Debug)]
pub struct SimulationFailure {
    /// The error that caused the failure
    pub error: OrderErr,
    /// If Some, this order needs an ACE unlock from this contract before it can succeed.
    /// The order should be queued for re-simulation once the unlock tx is available.
    pub ace_dependency: Option<Address>,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum OrderSimResult {
    /// Order simulated successfully
    Success(Arc<SimulatedOrder>, Vec<(Address, u64)>),
    /// Order simulation failed
    Failed(SimulationFailure),
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
    /// ACE contracts already provided as unlock parents (for progressive multi-ACE discovery)
    ace_unlock_contracts: HashSet<Address>,
}

pub type SimulationId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationRequest {
    pub id: SimulationId,
    pub order: Order,
    pub parents: Vec<Order>,
    /// ACE contracts for which we've already provided unlock parents.
    /// Used to determine if a failure is genuine (contract already unlocked) or needs retry.
    /// Supports multiple ACE contracts - order can progressively discover needed unlocks.
    pub ace_unlock_contracts: HashSet<Address>,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SimulatedResult {
    /// Successful simulation
    Success {
        id: SimulationId,
        simulated_order: Arc<SimulatedOrder>,
        previous_orders: Vec<Order>,
        /// Dependencies this simulation satisfies (nonces updated, ACE unlocks provided)
        dependencies_satisfied: Vec<DependencyKey>,
        simulation_time: Duration,
    },
    /// Order simulation failed
    Failed {
        id: SimulationId,
        order: Order,
        failure: SimulationFailure,
        /// ACE contracts that were already provided as unlock parents (preserved for re-queuing)
        ace_unlock_contracts: HashSet<Address>,
        simulation_time: Duration,
    },
}

/// Minimal data stored for completed simulations (to avoid Clone on full SimulatedResult)
#[derive(Debug, Clone)]
struct StoredSimulation {
    previous_orders: Vec<Order>,
    simulated_order: Arc<SimulatedOrder>,
}

// @Feat replaceable orders
#[derive(Debug)]
pub struct SimTree {
    // fields for nonce management
    nonces: NonceCache,

    sims: HashMap<SimulationId, StoredSimulation>,
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
                        ace_unlock_contracts: HashSet::default(),
                    },
                );
            }
            OrderDependencyState::Ready(parents) => {
                self.ready_orders.push(SimulationRequest {
                    id: rand::random(),
                    order,
                    parents,
                    ace_unlock_contracts: HashSet::default(),
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
                        let Some(sim) = self.sims.get(sim_id) else {
                            error!("SimTree bug: dependency provider sim not found");
                            pending_deps.push(dep_key);
                            continue;
                        };
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
    /// Called after simulation when we detect a NonUnlocking ACE interaction.
    /// Supports progressive multi-ACE discovery - existing_ace_unlock_contracts contains
    /// contracts we've already provided unlocks for in previous sim attempts.
    pub fn add_ace_dependency_for_order(
        &mut self,
        order: Order,
        new_contract: Address,
        mut existing_ace_unlock_contracts: HashSet<Address>,
    ) -> Result<(), ProviderError> {
        // Add new contract to the set
        existing_ace_unlock_contracts.insert(new_contract);
        let dep_key = DependencyKey::AceUnlock(new_contract);

        // Check if we already have an unlock provider for the new contract
        if self.dependency_providers.contains_key(&dep_key) {
            // Build parents from ALL ACE unlock contracts we need
            let mut parents = Vec::new();
            for contract in &existing_ace_unlock_contracts {
                let key = DependencyKey::AceUnlock(*contract);
                if let Some(sim_id) = self.dependency_providers.get(&key) {
                    if let Some(sim) = self.sims.get(sim_id) {
                        parents.extend(sim.previous_orders.clone());
                        parents.push(sim.simulated_order.order.clone());
                    }
                }
            }

            // Order is ready with all unlock txs as parents
            self.ready_orders.push(SimulationRequest {
                id: rand::random(),
                order,
                parents,
                ace_unlock_contracts: existing_ace_unlock_contracts,
            });
            return Ok(());
        }

        // New unlock not yet available - add to pending
        self.add_order_to_pending_with_ace(order, dep_key, existing_ace_unlock_contracts)
    }

    /// Helper to add an order to pending state with ACE unlock tracking
    fn add_order_to_pending_with_ace(
        &mut self,
        order: Order,
        dep_key: DependencyKey,
        ace_unlock_contracts: HashSet<Address>,
    ) -> Result<(), ProviderError> {
        self.pending_dependencies
            .entry(dep_key)
            .or_default()
            .push(order.id());
        self.pending_orders.insert(
            order.id(),
            PendingOrder {
                order,
                unsatisfied_dependencies: 1,
                ace_unlock_contracts,
            },
        );
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
        result: &SimulatedResult,
    ) -> Result<(), ProviderError> {
        let SimulatedResult::Success {
            id,
            simulated_order,
            previous_orders,
            dependencies_satisfied,
            ..
        } = result
        else {
            // Only Success variants should be processed here
            return Ok(());
        };

        self.sims.insert(
            *id,
            StoredSimulation {
                previous_orders: previous_orders.clone(),
                simulated_order: simulated_order.clone(),
            },
        );
        // Track orders that become ready along with their ACE state
        let mut orders_ready: Vec<PendingOrder> = Vec::new();
        let mut ace_unlock_contract: Option<Address> = None;

        // Process each dependency this simulation satisfies
        for dep_key in dependencies_satisfied.iter().cloned() {
            // Track if this dependency is an ACE unlock and which contract
            if let DependencyKey::AceUnlock(contract) = dep_key {
                ace_unlock_contract = Some(contract);
            }

            match self.dependency_providers.entry(dep_key.clone()) {
                Entry::Occupied(mut entry) => {
                    // Already have a provider - check if this one is more profitable
                    let current_sim_profit = {
                        let sim_id = entry.get_mut();
                        if let Some(existing_sim) = self.sims.get(sim_id) {
                            existing_sim
                                .simulated_order
                                .sim_value
                                .full_profit_info()
                                .coinbase_profit()
                        } else {
                            continue;
                        }
                    };
                    if simulated_order
                        .sim_value
                        .full_profit_info()
                        .coinbase_profit()
                        > current_sim_profit
                    {
                        entry.insert(*id);
                    }
                }
                Entry::Vacant(entry) => {
                    // First provider for this dependency
                    entry.insert(*id);

                    // Unblock orders waiting on this dependency
                    if let Some(pending_order_ids) = self.pending_dependencies.remove(&dep_key) {
                        for order_id in pending_order_ids {
                            match self.pending_orders.entry(order_id) {
                                Entry::Occupied(mut entry) => {
                                    let pending_order = entry.get_mut();
                                    pending_order.unsatisfied_dependencies -= 1;
                                    if pending_order.unsatisfied_dependencies == 0 {
                                        orders_ready.push(entry.remove());
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

        for mut ready_pending_order in orders_ready {
            let pending_state = self.get_order_dependency_state(&ready_pending_order.order)?;
            match pending_state {
                OrderDependencyState::Ready(mut parents) => {
                    // If this order became ready due to ACE unlock, add the unlock tx as parent
                    // and track the contract in ace_unlock_contracts
                    if let Some(contract) = ace_unlock_contract {
                        ready_pending_order.ace_unlock_contracts.insert(contract);
                        parents.extend(previous_orders.iter().cloned());
                        parents.push(simulated_order.order.clone());
                    }
                    self.ready_orders.push(SimulationRequest {
                        id: rand::random(),
                        order: ready_pending_order.order,
                        parents,
                        ace_unlock_contracts: ready_pending_order.ace_unlock_contracts,
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

    /// Handle ACE unlocking interaction after successful simulation.
    /// Returns optional cancellation OrderId if a mempool unlock cancels an optional ACE tx.
    /// Note: NonUnlocking ACE interactions are handled at the OrderSimResult level.
    pub fn handle_ace_unlock(
        &mut self,
        result: &mut SimulatedResult,
    ) -> Result<Option<OrderId>, ProviderError> {
        let SimulatedResult::Success {
            simulated_order,
            previous_orders,
            ..
        } = result
        else {
            return Ok(None);
        };

        let Some(AceInteraction::Unlocking {
            contract_address,
            source,
        }) = simulated_order.ace_interaction
        else {
            return Ok(None);
        };

        // If this order already has parents, it was re-simulated - just pass through
        if !previous_orders.is_empty() {
            return Ok(None);
        }

        // Register the unlock in ACE state based on source type
        let cancellation = match source {
            AceUnlockSource::ProtocolForce => {
                let state = self.ace_state.entry(contract_address).or_default();
                state.force_unlock_order = Some(simulated_order.clone());
                trace!(
                    "Added forced ACE protocol unlock order for {:?}",
                    contract_address
                );
                None
            }
            AceUnlockSource::ProtocolOptional => {
                let state = self.ace_state.entry(contract_address).or_default();

                // Check if user unlock already available - cancel optional
                if state.has_mempool_unlock {
                    trace!(
                        "Cancelling optional ACE unlock for {:?} - user unlock exists",
                        contract_address
                    );
                    return Ok(Some(simulated_order.order.id()));
                }

                // Only include optional if there are orders waiting on this unlock
                let dep_key = DependencyKey::AceUnlock(contract_address);
                if !self.pending_dependencies.contains_key(&dep_key) {
                    trace!(
                        "Cancelling optional ACE unlock for {:?} - no pending orders need it",
                        contract_address
                    );
                    return Ok(Some(simulated_order.order.id()));
                }

                // Store optional unlock - there are orders waiting for it
                state.optional_unlock_order = Some(simulated_order.clone());
                trace!(
                    "Added optional ACE protocol unlock order for {:?}",
                    contract_address
                );
                None
            }
            AceUnlockSource::User => {
                // A user unlocked ACE via mempool - mark it and cancel any optional protocol order
                trace!("User mempool unlock detected for {:?}", contract_address);
                self.mark_mempool_unlock(contract_address)
            }
        };

        Ok(cancellation)
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

    /// Process simulation results, handling ACE unlocks and updating dependencies.
    /// Returns:
    /// - `Vec<SimulatedResult>`: Successful results (to be forwarded to builder)
    /// - `Vec<OrderId>`: Order IDs that should be cancelled (e.g., optional ACE unlocks superseded by mempool)
    pub fn submit_simulation_tasks_results(
        &mut self,
        results: Vec<SimulatedResult>,
    ) -> Result<(Vec<SimulatedResult>, Vec<OrderId>), ProviderError> {
        let mut cancellations = Vec::new();
        let mut successful_results = Vec::with_capacity(results.len());

        for result in results {
            match result {
                SimulatedResult::Success { .. } => {
                    let mut result = result;
                    if let Some(id) = self.handle_ace_unlock(&mut result)? {
                        cancellations.push(id);
                    }
                    // All successful results need to be processed for dependency tracking
                    self.process_simulation_task_result(&result)?;
                    successful_results.push(result);
                }
                SimulatedResult::Failed {
                    order,
                    failure:
                        SimulationFailure {
                            ace_dependency: Some(contract_address),
                            ..
                        },
                    ace_unlock_contracts,
                    ..
                } => {
                    // Order failed but needs ACE unlock - queue for re-simulation
                    // Pass existing ace_unlock_contracts to support progressive multi-ACE discovery
                    self.add_ace_dependency_for_order(
                        order,
                        contract_address,
                        ace_unlock_contracts,
                    )?;
                }
                SimulatedResult::Failed { .. } => {
                    // Permanent failure - nothing to do
                }
            }
        }

        Ok((successful_results, cancellations))
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
                &sim_task.ace_unlock_contracts,
            )?;
            let (_, provider) = block_state.into_parts();
            state_for_sim = provider;
            match sim_result.result {
                OrderSimResult::Failed(failure) => {
                    if let Some(contract_address) = failure.ace_dependency {
                        // Order failed but needs ACE unlock - queue for re-simulation
                        // Pass existing ace_unlock_contracts for progressive multi-ACE discovery
                        sim_tree.add_ace_dependency_for_order(
                            sim_task.order,
                            contract_address,
                            sim_task.ace_unlock_contracts,
                        )?;
                    } else {
                        // Permanent failure
                        trace!(
                            order = sim_task.order.id().to_string(),
                            ?failure,
                            "Order simulation failed"
                        );
                        sim_errors.push(failure.error);
                    }
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

                    let result = SimulatedResult::Success {
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
        // For batch simulation, we ignore cancellations since there's no live processing
        let (_, _cancellations) = sim_tree.submit_simulation_tasks_results(sim_results)?;
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
    ace_unlock_contracts: &HashSet<Address>,
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
        ace_unlock_contracts,
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
    ace_unlock_contracts: &HashSet<Address>,
) -> Result<OrderSimResult, CriticalCommitOrderError> {
    let start = Instant::now();

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
                return Ok(OrderSimResult::Failed(SimulationFailure {
                    error: err,
                    ace_dependency: None,
                }));
            }
        }
    }

    // simulate
    let result = fork.commit_order(&order, space_state, true, &combined_refunds)?;
    let sim_time = start.elapsed();
    let sim_success = result.is_ok();
    add_order_simulation_time(sim_time, "sim", sim_success); // we count parent sim time + order sim time time here

    // Get the used_state_trace from tracer (available regardless of success/failure)
    let used_state_trace = fork.tracer.as_mut().and_then(|t| t.take_used_state_trace());

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
            classify_ace_interaction(trace, sim_success, config, selector, tx_to)
        })
    });

    match result {
        Ok(res) => {
            let sim_value = create_sim_value(&order, &res, mempool_tx_detector);
            if let Err(err) = order_is_worth_executing(&sim_value) {
                return Ok(OrderSimResult::Failed(SimulationFailure {
                    error: err,
                    ace_dependency: None,
                }));
            }
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
            // Check if failed order accessed ACE - may need re-simulation with unlock parent
            let ace_dependency = if let Some(AceInteraction::NonUnlocking { contract_address }) =
                ace_interaction
            {
                if ace_unlock_contracts.contains(&contract_address) {
                    // Already had unlock for this contract but still failed - genuine failure
                    tracing::debug!(
                        order = ?order.id(),
                        ?err,
                        ?contract_address,
                        "Order failed despite having ACE unlock for this contract - genuine failure"
                    );
                    None
                } else {
                    // Need unlock for this contract (might already have others)
                    tracing::debug!(
                        order = ?order.id(),
                        ?err,
                        ?contract_address,
                        existing_unlocks = ?ace_unlock_contracts,
                        "Order needs additional ACE unlock"
                    );
                    Some(contract_address)
                }
            } else {
                None
            };

            Ok(OrderSimResult::Failed(SimulationFailure {
                error: err,
                ace_dependency,
            }))
        }
    }
}
