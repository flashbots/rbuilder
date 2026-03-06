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
use itertools::Itertools;
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
    pub ace_state: AceSimulationState,
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
        // Because we only expect one or the other, and force tx will always be at the top of the
        // block. we want to ensure we always select the correct order for what we expect in the
        // block builder.
        self.force_unlock_order
            .as_ref()
            .or_else(|| self.optional_unlock_order.as_ref())
    }
}

/// Tracks ACE simulation state for an order through iterative re-simulations.
///
/// Key concepts:
/// - NonUnlocking interactions = need unlock parents (revert without)
/// - Unlocking interactions = ARE the unlock parents (never need parents themselves)
///
/// Flow:
/// 1) Simulate order + collect possible ACE interactions
/// 2) If NonUnlocking interactions exist, wait for unlock parents
/// 3) Re-simulate with ACE context, compute symmetric difference of
///    new NonUnlocking dependencies vs already-accounted-for interactions
/// 4) If unhandled set is empty -> done, else -> repeat from 3
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AceSimulationState {
    /// ACE interactions detected
    /// Includes both Unlocking (parent providers) and NonUnlocking (need parents).
    pub detected_interactions: HashSet<AceInteraction>,

    /// ACE interactions (by contract address) for which we've already provided
    /// unlock parents in previous simulation attempts.
    pub accounted_for_interactions: HashSet<AceInteraction>,
}

impl AceSimulationState {
    /// Returns NonUnlocking interactions that still need unlock parents.
    /// Filters out interactions where we already have an Unlocking interaction
    /// for the same contract in accounted_for_interactions.
    pub fn dependencies_to_handle(&self) -> HashSet<AceInteraction> {
        // Get contract addresses for which we have unlocks accounted for
        let accounted_contracts: HashSet<Address> = self
            .accounted_for_interactions
            .iter()
            .filter(|i| i.is_unlocking())
            .map(|i| i.get_contract_address())
            .collect();

        // Return NonUnlocking interactions whose contracts aren't yet accounted for
        self.detected_interactions
            .iter()
            .filter(|i| i.needs_unlock())
            .filter(|i| !accounted_contracts.contains(&i.get_contract_address()))
            .copied()
            .collect()
    }

    pub fn all_dependencies_accounted(&self) -> bool {
        self.dependencies_to_handle().is_empty()
    }

    pub fn add_accounted_interactions(&mut self, actions: impl Iterator<Item = AceInteraction>) {
        self.accounted_for_interactions.extend(actions)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingOrder {
    order: Arc<Order>,
    /// ACE state tracking detected and accounted-for interactions
    ace_state: AceSimulationState,
}

pub type SimulationId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationRequest {
    pub id: SimulationId,
    pub order: Arc<Order>,
    pub parents: Vec<Arc<Order>>,
    /// ACE contracts for which we've already provided unlock parents.
    /// Used to determine if a failure is genuine (contract already unlocked) or needs retry.
    /// Supports multiple ACE contracts - order can progressively discover needed unlocks.
    pub ace_state: AceSimulationState,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SimulatedResult {
    /// Successful simulation
    Success {
        id: SimulationId,
        simulated_order: Arc<SimulatedOrder>,
        previous_orders: Vec<Arc<Order>>,
        /// Dependencies this simulation satisfies (nonces updated, ACE unlocks provided)
        dependencies_satisfied: Vec<DependencyKey>,
        simulation_time: Duration,
    },
    /// Order simulation failed
    Failed {
        id: SimulationId,
        order: Arc<Order>,
        failure: SimulationFailure,
        simulation_time: Duration,
    },
}

impl SimulatedResult {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }
}

/// Minimal data stored for completed simulations (to avoid Clone on full SimulatedResult)
#[derive(Debug, Clone)]
struct StoredSimulation {
    // parents
    parent_orders: Vec<Arc<Order>>,
    // result
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
    Ready(Vec<Arc<Order>>),
}

impl SimTree {
    pub fn new(nonce_cache_ref: NonceCache, ace_configs: Vec<AceConfig>) -> Self {
        let mut ace_config = HashMap::default();
        let mut ace_state = HashMap::default();

        if ace_configs.is_empty() {
            tracing::debug!("ACE SimTree: initialized with no ACE configs");
        } else {
            tracing::debug!(
                ace_config_count = ace_configs.len(),
                "ACE SimTree: initializing with ACE configs"
            );
        }

        for config in ace_configs {
            let contract_address = config.contract_address;
            tracing::debug!(
                contract_address = ?contract_address,
                detection_slots = ?config.detection_slots,
                "ACE SimTree: registered protocol"
            );
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

    fn push_order(&mut self, order: Arc<Order>) -> Result<(), ProviderError> {
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
                        ace_state: AceSimulationState::default(),
                    },
                );
            }
            OrderDependencyState::Ready(parents) => {
                self.ready_orders.push(SimulationRequest {
                    id: rand::random(),
                    order,
                    parents,
                    // we don't have a state for it yet.
                    ace_state: AceSimulationState::default(),
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
                        parent_orders.extend_from_slice(&sim.parent_orders);
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

    /// Takes a failed ace dependency order and adds parents / puts into holding for parents to
    /// arrive.
    pub fn handle_ace_dependencies_for_order(
        &mut self,
        order: Arc<Order>,
        mut ace_state: AceSimulationState,
    ) {
        let order_id = order.id();
        let difference = ace_state.dependencies_to_handle();
        // If we have handled all dependencies, this order will not be valid and thus we will
        // ignore it.
        if difference.is_empty() {
            tracing::debug!(
                order_id = ?order_id,
                "ACE deps: order has no unhandled ACE dependencies, ignoring"
            );
            return;
        }

        tracing::debug!(
            order_id = ?order_id,
            dependency_count = difference.len(),
            dependencies = ?difference,
            "ACE deps: handling ACE dependencies for failed order"
        );

        let mut is_ready = true;

        let keys = difference
            .into_iter()
            .filter_map(|dep| {
                let dep_key = DependencyKey::AceUnlock(dep.get_contract_address());

                match self.dependency_providers.get(&dep_key) {
                    Some(key) => {
                        tracing::debug!(
                            order_id = ?order_id,
                            contract_address = ?dep.get_contract_address(),
                            "ACE deps: found existing unlock provider for dependency"
                        );
                        Some(key)
                    }

                    None => {
                        tracing::debug!(
                            order_id = ?order_id,
                            contract_address = ?dep.get_contract_address(),
                            "ACE deps: no unlock provider found, order waiting for unlock parent"
                        );
                        is_ready = false;
                        self.pending_dependencies
                            .entry(dep_key)
                            .or_default()
                            .push(order.id());
                        None
                    }
                }
            })
            .collect_vec();

        // Order needs to wait for ACE unlock dependencies
        if !is_ready {
            tracing::debug!(
                order_id = ?order_id,
                "ACE deps: order added to pending, waiting for unlock parents"
            );
            self.pending_orders
                .insert(order.id(), PendingOrder { order, ace_state });
            return;
        }

        let parents = keys
            .into_iter()
            .filter_map(|sim_id| {
                // for each parent, we want to track it now.
                if let Some(sim) = self.sims.get(sim_id) {
                    ace_state.add_accounted_interactions(
                        sim.simulated_order.ace_interactions.iter().copied(),
                    );

                    let mut parents = sim.parent_orders.clone();
                    parents.push(sim.simulated_order.order.clone());

                    Some(parents)
                } else {
                    None
                }
            })
            .flatten()
            .collect::<Vec<_>>();

        tracing::debug!(
            order_id = ?order_id,
            parent_count = parents.len(),
            "ACE deps: order ready with unlock parents, queued for re-simulation"
        );

        // Order is ready with all unlock txs as parents
        self.ready_orders.push(SimulationRequest {
            id: rand::random(),
            order,
            parents,
            ace_state,
        });
    }

    pub fn push_orders(&mut self, orders: Vec<Arc<Order>>) -> Result<(), ProviderError> {
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
            previous_orders,
            simulated_order,
            dependencies_satisfied,
            id,
            ..
        } = result
        else {
            // Only Success variants should be processed here
            return Ok(());
        };

        self.sims.insert(
            *id,
            StoredSimulation {
                parent_orders: previous_orders.clone(),
                simulated_order: simulated_order.clone(),
            },
        );

        // Track orders that become ready (all deps satisfied)
        let mut orders_ready: Vec<PendingOrder> = Vec::new();

        // Process each dependency this simulation satisfies
        for dep_key in dependencies_satisfied.iter().cloned() {
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

                    // Update orders waiting on this dependency
                    if let Some(pending_order_ids) = self.pending_dependencies.remove(&dep_key) {
                        for order_id in pending_order_ids {
                            if let Entry::Occupied(mut entry) = self.pending_orders.entry(order_id)
                            {
                                let pending_order = entry.get_mut();

                                // Add the unlock interactions to the order's accounted_for set
                                if matches!(dep_key, DependencyKey::AceUnlock(_)) {
                                    pending_order.ace_state.add_accounted_interactions(
                                        simulated_order.ace_interactions.iter().copied(),
                                    );
                                }

                                // Check if all ACE deps are now accounted for
                                if pending_order.ace_state.all_dependencies_accounted() {
                                    orders_ready.push(entry.remove());
                                }
                                // Otherwise order stays pending, waiting for more deps
                            }
                        }
                    }
                }
            }
        }

        // Process orders that are now fully ready
        for ready_pending_order in orders_ready {
            let pending_state = self.get_order_dependency_state(&ready_pending_order.order)?;
            match pending_state {
                OrderDependencyState::Ready(mut parents) => {
                    let ace_state = ready_pending_order.ace_state;

                    // Collect ALL ACE parent orders from dependency_providers
                    for interaction in ace_state.detected_interactions.iter() {
                        if interaction.needs_unlock() {
                            let dep_key =
                                DependencyKey::AceUnlock(interaction.get_contract_address());
                            if let Some(sim_id) = self.dependency_providers.get(&dep_key) {
                                if let Some(sim) = self.sims.get(sim_id) {
                                    parents.extend(sim.parent_orders.iter().cloned());
                                    parents.push(sim.simulated_order.order.clone());
                                }
                            }
                        }
                    }

                    self.ready_orders.push(SimulationRequest {
                        id: rand::random(),
                        order: ready_pending_order.order,
                        parents,
                        ace_state,
                    });
                }
                OrderDependencyState::Invalid => {
                    error!("SimTree bug order became invalid");
                }
                OrderDependencyState::Pending(_) => {
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
        result: &SimulatedResult,
    ) -> Result<Vec<OrderId>, ProviderError> {
        let SimulatedResult::Success {
            simulated_order,
            previous_orders,
            ..
        } = result
        else {
            return Ok(Vec::new());
        };

        // If this order already has parents, it was re-simulated - just pass through
        if !previous_orders.is_empty() {
            return Ok(Vec::new());
        }

        // Get all unlocking interactions
        let unlocking_interactions: Vec<_> = simulated_order
            .ace_interactions
            .iter()
            .filter_map(|i| match i {
                AceInteraction::Unlocking {
                    contract_address,
                    source,
                } => Some((*contract_address, *source)),
                AceInteraction::NonUnlocking { .. } => None,
            })
            .collect();

        if unlocking_interactions.is_empty() {
            return Ok(Vec::new());
        }

        let mut cancellations = Vec::new();

        // Process each unlocking interaction
        for (contract_address, source) in unlocking_interactions {
            match source {
                AceUnlockSource::ProtocolForce => {
                    let state = self.ace_state.entry(contract_address).or_default();
                    state.force_unlock_order = Some(simulated_order.clone());
                    trace!(
                        "Added forced ACE protocol unlock order for {:?}",
                        contract_address
                    );
                }
                AceUnlockSource::ProtocolOptional => {
                    let state = self.ace_state.entry(contract_address).or_default();

                    // Check if user unlock already available - cancel optional
                    if state.has_mempool_unlock {
                        trace!(
                            "Cancelling optional ACE unlock for {:?} - user unlock exists",
                            contract_address
                        );
                        cancellations.push(simulated_order.order.id());
                        continue;
                    }

                    // Only include optional if there are orders waiting on this unlock
                    let dep_key = DependencyKey::AceUnlock(contract_address);
                    if !self.pending_dependencies.contains_key(&dep_key) {
                        trace!(
                            "Cancelling optional ACE unlock for {:?} - no pending orders need it",
                            contract_address
                        );
                        cancellations.push(simulated_order.order.id());
                        continue;
                    }

                    // Store optional unlock - there are orders waiting for it
                    state.optional_unlock_order = Some(simulated_order.clone());
                    trace!(
                        "Added optional ACE protocol unlock order for {:?}",
                        contract_address
                    );
                }
                AceUnlockSource::User => {
                    // A user unlocked ACE via mempool - mark it and cancel any optional protocol order
                    trace!("User mempool unlock detected for {:?}", contract_address);
                    if let Some(cancelled_id) = self.mark_mempool_unlock(contract_address) {
                        cancellations.push(cancelled_id);
                    }
                }
            }
        }

        Ok(cancellations)
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
            if let SimulatedResult::Success { .. } = result {
                cancellations.extend(self.handle_ace_unlock(&result)?);
                // All successful results need to be processed for dependency tracking
                self.process_simulation_task_result(&result)?;
                successful_results.push(result);
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
    orders: &[Arc<Order>],
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
        sim_tree.push_orders(std::mem::take(&mut orders))?;
    }

    let sim_errors = Vec::new();
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
                &sim_task.ace_state,
            )?;
            let (_, provider) = block_state.into_parts();
            state_for_sim = provider;
            match sim_result.result {
                OrderSimResult::Failed(failure) => {
                    // if we have a failure, we will handle the case were its ace by either putting
                    // it into pending or requeing if we have the deps. Otherwise, no action is
                    // taken and flow handles as normal.
                    sim_tree.handle_ace_dependencies_for_order(sim_task.order, failure.ace_state);
                }
                OrderSimResult::Success(sim_order, nonces) => {
                    let mut dependencies_satisfied: Vec<DependencyKey> = nonces
                        .into_iter()
                        .map(|(address, nonce)| DependencyKey::Nonce(NonceKey { address, nonce }))
                        .collect();

                    // Add ACE dependencies for all unlocking interactions
                    for interaction in &sim_order.ace_interactions {
                        if let AceInteraction::Unlocking {
                            contract_address, ..
                        } = interaction
                        {
                            dependencies_satisfied
                                .push(DependencyKey::AceUnlock(*contract_address));
                        }
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
    parent_orders: Vec<Arc<Order>>,
    order: Arc<Order>,
    ctx: &BlockBuildingContext,
    local_ctx: &mut ThreadBlockBuildingContext,
    state: &mut BlockState,
    ace_configs: &HashMap<Address, AceConfig>,
    // we have parents for these ace addresses.
    current_ace_state: &AceSimulationState,
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
        current_ace_state,
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
    parent_orders: Vec<Arc<Order>>,
    order: Arc<Order>,
    fork: &mut PartialBlockFork<'_, '_, '_, '_, Tracer, NullPartialBlockForkExecutionTracer>,
    mempool_tx_detector: &MempoolTxsDetector,
    ace_configs: &HashMap<Address, AceConfig>,
    current_ace_state: &AceSimulationState,
) -> Result<OrderSimResult, CriticalCommitOrderError> {
    let start = Instant::now();

    // simulate parents
    let mut space_state = BlockBuildingSpaceState::ZERO;
    // We use empty combined refunds because the value of the bundle will
    // not change from batching.
    let combined_refunds = std::collections::HashMap::default();
    for parent in &parent_orders {
        let result = fork.commit_order(parent, space_state, true, &combined_refunds)?;
        match result {
            Ok(res) => {
                space_state.use_space(res.space_used);
            }
            Err(err) => {
                tracing::trace!(parent_order = ?parent.id(), ?err, "failed to simulate parent order");
                return Ok(OrderSimResult::Failed(SimulationFailure {
                    error: err,
                    // Given a parent failure. We will return a empty ace simulation state as this
                    // signals to treat it as a regular order.
                    ace_state: AceSimulationState::default(),
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

    // Detect ACE interactions from the state trace using config
    // Check ALL transactions in the order and collect ALL ACE interactions (one per contract)
    // For each contract, keep the highest priority classification:
    // Priority: ProtocolForce > ProtocolOptional > User > NonUnlocking
    let ace_interactions: Vec<AceInteraction> = if let Some(trace) = used_state_trace.as_ref() {
        // Use HashMap to track best interaction per contract
        let mut per_contract: HashMap<Address, AceInteraction> = HashMap::default();

        for (tx, _) in order.list_txs() {
            let input = tx.internal_tx_unsecure().input();
            let selector = if input.len() >= 4 {
                Some(Selector::from_slice(&input[..4]))
            } else {
                None
            };
            let tx_to = tx.to();
            let tx_from = Some(tx.signer());

            // Check this transaction against all ACE configs
            for (_, config) in ace_configs.iter() {
                if let Some(interaction) =
                    classify_ace_interaction(trace, sim_success, config, selector, tx_to, tx_from)
                {
                    let contract = interaction.get_contract_address();
                    // Update if new interaction has higher priority for this contract
                    per_contract
                        .entry(contract)
                        .and_modify(|existing| {
                            if interaction_priority(&interaction) > interaction_priority(existing) {
                                *existing = interaction;
                            }
                        })
                        .or_insert(interaction);
                }
            }
        }

        per_contract.into_values().collect()
    } else {
        Vec::new()
    };

    // Log ACE interactions detected for this order
    if !ace_interactions.is_empty() {
        for interaction in &ace_interactions {
            tracing::debug!(
                order_id = ?order.id(),
                sim_success = sim_success,
                ace_interaction = ?interaction,
                "ACE sim: detected interaction for order"
            );
        }
    }

    match result {
        Ok(res) => {
            let sim_value = create_sim_value(&order, &res, mempool_tx_detector);

            // Check if this is an ACE protocol unlock order (ProtocolForce or ProtocolOptional)
            // These orders may have zero profit but are valuable for enabling other transactions
            let is_ace_protocol_unlock = ace_interactions.iter().any(|i| i.is_protocol_tx());

            if let Err(err) = order_is_worth_executing(&sim_value) {
                if is_ace_protocol_unlock {
                    // ACE protocol unlocks bypass profit check - their value is enabling other txs
                    tracing::debug!(
                        order_id = ?order.id(),
                        ace_interactions = ?ace_interactions,
                        "ACE sim: protocol unlock order bypassing profit check"
                    );
                } else {
                    // Not an ACE protocol unlock, reject as usual
                    return Ok(OrderSimResult::Failed(SimulationFailure {
                        error: err,
                        ace_state: AceSimulationState::default(),
                    }));
                }
            }
            let new_nonces = res.nonces_updated.into_iter().collect::<Vec<_>>();
            let mut simulated_order = SimulatedOrder::new(order, sim_value, res.used_state_trace);
            simulated_order.ace_interactions = ace_interactions;
            Ok(OrderSimResult::Success(
                Arc::new(simulated_order),
                new_nonces,
            ))
        }
        Err(err) => {
            // Build ACE state with only NonUnlocking interactions (they need unlock parents)
            let non_unlocking: HashSet<AceInteraction> = ace_interactions
                .iter()
                .filter(|i| i.needs_unlock())
                .copied()
                .collect();

            // Log failed orders that have ACE dependencies
            if !non_unlocking.is_empty() {
                tracing::debug!(
                    order_id = ?order.id(),
                    error = ?err,
                    non_unlocking_count = non_unlocking.len(),
                    non_unlocking_interactions = ?non_unlocking,
                    "ACE sim: order failed with non-unlocking ACE interactions (needs unlock parent)"
                );
            }

            Ok(OrderSimResult::Failed(SimulationFailure {
                error: err,
                ace_state: AceSimulationState {
                    detected_interactions: non_unlocking,
                    accounted_for_interactions: current_ace_state
                        .accounted_for_interactions
                        .clone(),
                },
            }))
        }
    }
}

/// Returns priority score for ACE interaction (higher = more important)
fn interaction_priority(interaction: &AceInteraction) -> u8 {
    match interaction {
        AceInteraction::Unlocking {
            source: AceUnlockSource::ProtocolForce,
            ..
        } => 4,
        AceInteraction::Unlocking {
            source: AceUnlockSource::ProtocolOptional,
            ..
        } => 3,
        AceInteraction::Unlocking {
            source: AceUnlockSource::User,
            ..
        } => 2,
        AceInteraction::NonUnlocking { .. } => 1,
    }
}
