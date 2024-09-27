mod cli;
mod redistribution_algo;

use crate::backtest::execute::backtest_simulate_block;
use crate::backtest::redistribute::redistribution_algo::{
    IncludedOrderData, RedistributionCalculator, RedistributionIdentityData, RedistributionResult,
};
use crate::backtest::restore_landed_orders::{
    restore_landed_orders, sim_historical_block, ExecutedBlockTx, LandedOrderData, SimplifiedOrder,
};
use crate::backtest::{BlockData, BuiltBlockData, OrdersWithTimestamp};
use crate::live_builder::cli::LiveBuilderConfig;
use crate::primitives::{Order, OrderId};
use crate::utils::signed_uint_delta;
use crate::utils::u256decimal_serde_helper;
use ahash::{HashMap, HashSet};
use alloy_primitives::utils::format_ether;
use alloy_primitives::{Address, B256, I256, U256};
pub use cli::run_backtest_redistribute;
use rayon::prelude::*;
use reth_chainspec::ChainSpec;
use reth_db::DatabaseEnv;
use reth_provider::ProviderFactory;
use serde::{Deserialize, Serialize};
use std::cmp::{max, min};
use std::sync::Arc;
use tracing::{debug, info, info_span, trace, warn};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityData {
    pub address: Address,
    #[serde(with = "u256decimal_serde_helper")]
    pub redistribution_value_received: U256,
    pub block_value_delta: I256,
    pub inclusion_changes: Vec<OrderInclusionChange>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderInclusionChange {
    id: ExtendedOrderId,
    change: InclusionChange,
    #[serde(with = "u256decimal_serde_helper")]
    profit_before: U256,
    #[serde(with = "u256decimal_serde_helper")]
    profit_after: U256,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InclusionChange {
    Excluded,
    Included,
    ProfitChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExtendedOrderId {
    Tx(B256),
    Bundle { uuid: Uuid, hash: B256 },
    ShareBundle(B256),
}

impl ExtendedOrderId {
    fn new(order_id: OrderId, bundle_hashes: &HashMap<OrderId, B256>) -> Self {
        match order_id {
            OrderId::Tx(hash) => ExtendedOrderId::Tx(hash),
            OrderId::Bundle(uuid) => {
                let hash = bundle_hashes.get(&order_id).cloned().unwrap_or_default();
                ExtendedOrderId::Bundle { uuid, hash }
            }
            OrderId::ShareBundle(hash) => ExtendedOrderId::ShareBundle(hash),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderData {
    pub id: ExtendedOrderId,
    pub identity: Address,
    /// `sender` is signer of the transaction when order has only 1 tx
    /// otherwise its a signer of the request (e.g. eth_sendBundle)
    pub sender: Address,
    #[serde(with = "u256decimal_serde_helper")]
    pub redistribution_value_received: U256,
    pub block_value_delta: I256,
    pub inclusion_changes: Vec<OrderInclusionChange>,
    #[serde(with = "u256decimal_serde_helper")]
    pub realized_value: U256,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JointContributionData {
    pub identities: Vec<Address>,
    pub block_value_delta: I256,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedistributionBlockOutput {
    pub block_number: u64,
    pub block_hash: B256,
    #[serde(with = "u256decimal_serde_helper")]
    pub block_profit: U256,
    pub identities: Vec<IdentityData>,
    pub landed_orders: Vec<OrderData>,
    pub joint_contribution: Vec<JointContributionData>,
}

pub fn calc_redistributions<ConfigType: LiveBuilderConfig + Send + Sync>(
    provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
    config: &ConfigType,
    block_data: BlockData,
    distribute_to_mempool_txs: bool,
) -> eyre::Result<RedistributionBlockOutput> {
    let _block_span = info_span!("block", block = block_data.block_number).entered();
    let protect_signers = config.base_config().backtest_protect_bundle_signers.clone();

    let (onchain_block_profit, block_data, built_block_data) = prepare_block_data(block_data)?;

    let included_orders_available =
        get_available_orders(&block_data, &built_block_data, distribute_to_mempool_txs);

    let restored_landed_orders = restore_available_landed_orders(
        provider_factory.clone(),
        config.base_config().chain_spec()?,
        &block_data,
        &included_orders_available,
    )?;

    let available_orders = split_orders_by_identities(
        included_orders_available,
        &protect_signers,
        &block_data,
        distribute_to_mempool_txs,
    );

    let results_without_exclusion =
        calculate_backtest_without_exclusion(provider_factory.clone(), config, block_data.clone())?;

    let exclusion_results = calculate_backtest_identity_and_order_exclusion(
        provider_factory.clone(),
        config,
        block_data.clone(),
        &available_orders,
        &results_without_exclusion,
    )?;

    let exclusion_results = calc_joint_exclusion_results(
        provider_factory.clone(),
        config,
        block_data.clone(),
        &available_orders,
        &results_without_exclusion,
        exclusion_results,
    )?;

    let calculated_redistribution_result = apply_redistribution_formula(
        onchain_block_profit,
        &available_orders,
        &exclusion_results,
        &restored_landed_orders,
    );

    let result = collect_redistribution_result(
        &block_data,
        onchain_block_profit,
        available_orders,
        results_without_exclusion,
        exclusion_results,
        calculated_redistribution_result,
        &restored_landed_orders,
    );

    let redistributed_identity_value = result
        .identities
        .iter()
        .map(|i| i.redistribution_value_received)
        .sum::<U256>();
    let redistributed_orders_value = result
        .landed_orders
        .iter()
        .map(|o| o.redistribution_value_received)
        .sum::<U256>();

    assert!(
        redistributed_identity_value <= onchain_block_profit,
        "Redistributed identity value is greater than onchain block profit"
    );
    assert!(
        redistributed_orders_value <= onchain_block_profit,
        "Redistributed orders value is greater than onchain block profit"
    );

    Ok(result)
}

fn prepare_block_data(
    mut block_data: BlockData,
) -> eyre::Result<(U256, BlockData, BuiltBlockData)> {
    let built_block_data = if let Some(block_data) = block_data.built_block_data.clone() {
        block_data
    } else {
        warn!(block = block_data.block_number, "Block data not found");
        eyre::bail!("Included block data not found");
    };

    block_data.filter_orders_by_end_timestamp(built_block_data.orders_closed_at);
    // @TODO filter cancellations properly, for this we need actual cancellations in the backtest data
    // filter bundles made out of mempool txs
    block_data.filter_bundles_from_mempool();

    let block_profit = if built_block_data.profit.is_positive() {
        built_block_data.profit.into_sign_and_abs().1
    } else {
        U256::ZERO
    };
    Ok((block_profit, block_data, built_block_data))
}

fn get_available_orders(
    block_data: &BlockData,
    built_block_data: &BuiltBlockData,
    distribute_to_mempool_txs: bool,
) -> Vec<OrdersWithTimestamp> {
    let orders_by_id = block_data
        .available_orders
        .iter()
        .map(|order| (order.order.id(), order.clone()))
        .collect::<HashMap<_, _>>();

    let mut included_orders_available = HashMap::default();
    for id in &built_block_data.included_orders {
        match orders_by_id.get(id) {
            Some(order) => {
                included_orders_available.insert(order.order.id(), order.clone());
            }
            None => {
                warn!(order = ?id, "Included order not found in available orders");
            }
        }
    }
    if distribute_to_mempool_txs {
        for tx in block_data.onchain_block.transactions.hashes() {
            let id = OrderId::Tx(tx);
            if let Some(order) = orders_by_id.get(&id) {
                included_orders_available.insert(order.order.id(), order.clone());
            }
        }
    }
    let mut included_orders_available = included_orders_available.into_values().collect::<Vec<_>>();
    included_orders_available.sort_by_key(|order| order.order.id());
    included_orders_available
}

fn restore_available_landed_orders(
    provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
    chain_spec: Arc<ChainSpec>,
    block_data: &BlockData,
    included_orders_available: &[OrdersWithTimestamp],
) -> eyre::Result<HashMap<OrderId, LandedOrderData>> {
    let block_txs = sim_historical_block(
        provider_factory.clone(),
        chain_spec,
        block_data.onchain_block.clone(),
    )?
    .into_iter()
    .map(|executed_tx| {
        ExecutedBlockTx::new(
            executed_tx.hash(),
            executed_tx.coinbase_profit,
            executed_tx.receipt.success,
        )
    })
    .collect::<Vec<_>>();

    let mut simplified_orders = Vec::new();

    for available_order in included_orders_available {
        simplified_orders.push(SimplifiedOrder::new_from_order(&available_order.order));
    }

    // we always add mempool txs to the list of available orders
    // so that when restoring only unique and private txs are counted towards profit
    for available_order in &block_data.available_orders {
        if available_order.order.is_tx()
            && !included_orders_available
                .iter()
                .any(|o| o.order.id() == available_order.order.id())
        {
            simplified_orders.push(SimplifiedOrder::new_from_order(&available_order.order));
        }
    }
    Ok(restore_landed_orders(block_txs, simplified_orders))
}

#[derive(Debug)]
struct AvailableOrders {
    included_orders_available: Vec<OrdersWithTimestamp>,
    included_orders_by_address: Vec<(Address, Vec<OrderId>)>,
    all_orders_by_address: HashMap<Address, Vec<OrderId>>,
    orders_id_to_address: HashMap<OrderId, Address>,
    all_orders_by_id: HashMap<OrderId, Order>,
    bundle_hash_by_id: HashMap<OrderId, B256>,
    order_sender_by_id: HashMap<OrderId, Address>,
}

impl AvailableOrders {
    /// When calculating exclusion values for individual orders we want to exclude
    /// other conflicting orders from the same identity
    /// * we exclude orders itself
    /// * other orders from the same identity with conflicting nonces
    /// * other orders from the same identity with the same replacement uuid
    fn calc_individual_included_orders_exclusion(&self) -> HashMap<OrderId, Vec<OrderId>> {
        let get_nonces_and_replacement = |id| {
            let order = self
                .all_orders_by_id
                .get(id)
                .expect("order not found it all orders set");
            let mandatory_nonces: HashSet<Address> = order
                .nonces()
                .iter()
                .filter_map(|n| {
                    if n.optional {
                        return None;
                    }
                    Some(n.address)
                })
                .collect();
            (mandatory_nonces, order.replacement_key())
        };

        let mut result = HashMap::default();
        for (address, included_orders) in &self.included_orders_by_address {
            for included in included_orders {
                let mut order_exclusion = vec![*included];

                let (nonces, replacement) = get_nonces_and_replacement(included);

                for other_order in self
                    .all_orders_by_address
                    .get(address)
                    .expect("all orders by address not found")
                {
                    let (other_nonces, other_replacement) = get_nonces_and_replacement(other_order);
                    for nonce in &nonces {
                        if other_nonces.contains(nonce) {
                            order_exclusion.push(*other_order);
                        }
                    }
                    if replacement.is_some() && other_replacement == replacement {
                        order_exclusion.push(*other_order);
                    }
                }
                result.insert(*included, order_exclusion);
            }
        }
        result
    }
}

fn split_orders_by_identities(
    included_orders_available: Vec<OrdersWithTimestamp>,
    protect_signers: &[Address],
    block_data: &BlockData,
    distribute_to_mempool_txs: bool,
) -> AvailableOrders {
    let mut all_orders_by_address: HashMap<Address, Vec<OrderId>> = HashMap::default();
    let mut included_orders_by_address: HashMap<Address, Vec<OrderId>> = HashMap::default();
    let mut orders_id_to_address = HashMap::default();
    let mut bundle_hash_by_id = HashMap::default();
    let mut order_sender_by_id = HashMap::default();

    for order in &included_orders_available {
        let order_id = order.order.id();
        let address = match order_redistribution_address(&order.order, protect_signers) {
            Some(address) => address,
            None => {
                warn!(order = ?order_id, "Included order redistribution address not found");
                continue;
            }
        };
        info!(identity = ?address, order = ?order_id,"Available landed order");
        let orders = included_orders_by_address.entry(address).or_default();
        orders.push(order_id);
        orders_id_to_address.insert(order_id, address);
    }

    for order in &block_data.available_orders {
        let id = order.order.id();
        if let Order::Bundle(bundle) = &order.order {
            bundle_hash_by_id.insert(id, bundle.hash);
        };
        order_sender_by_id.insert(id, order_sender(&order.order));
        let address = match order_redistribution_address(&order.order, protect_signers) {
            Some(address) => address,
            None => {
                warn!(order = ?id, "Available order redistribution address not found");
                continue;
            }
        };
        let order_was_included = included_orders_by_address
            .get(&address)
            .map(|o| o.contains(&id))
            .unwrap_or_default();
        // the idea here is for the (real) situation where we have
        // 1. bundle from some address
        // 2. mempool tx from that address
        // 3. and we don't count mempool transactions
        // We don't count mempool tx by that address as sent by that identity
        // because mempool txs are not private
        if order.order.is_tx() && !distribute_to_mempool_txs && !order_was_included {
            continue;
        }
        let orders = all_orders_by_address.entry(address).or_default();
        orders.push(id);
        orders_id_to_address.insert(id, address);
    }

    let mut included_orders_by_address: Vec<(Address, Vec<OrderId>)> =
        included_orders_by_address.into_iter().collect();
    included_orders_by_address.sort_by_key(|(address, _)| *address);
    for (_, orders) in &mut included_orders_by_address {
        orders.sort();
    }

    AvailableOrders {
        included_orders_available,
        included_orders_by_address,
        all_orders_by_address,
        orders_id_to_address,
        all_orders_by_id: block_data
            .available_orders
            .iter()
            .map(|order| (order.order.id(), order.order.clone()))
            .collect(),
        bundle_hash_by_id,
        order_sender_by_id,
    }
}

#[derive(Debug)]
struct ResultsWithoutExclusion {
    profit: U256,
    orders_included: Vec<(OrderId, U256)>,
    orders_failed: Vec<OrderId>,
}

impl ResultsWithoutExclusion {
    fn exclusion_input(&self, exclude_orders: Vec<OrderId>) -> ExclusionInput {
        ExclusionInput {
            exclude_orders,
            orders_included_before: self.orders_included.to_vec(),
            orders_excluded_before: self.orders_failed.to_vec(),
            profit_before: self.profit,
        }
    }
}

fn calculate_backtest_without_exclusion<ConfigType: LiveBuilderConfig>(
    provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
    config: &ConfigType,
    block_data: BlockData,
) -> eyre::Result<ResultsWithoutExclusion> {
    let ExclusionResult {
        profit,
        new_orders_failed: orders_failed,
        new_orders_included: orders_included,
        ..
    } = calc_profit_after_exclusion(
        provider_factory.clone(),
        config,
        &block_data,
        ExclusionInput {
            exclude_orders: vec![],
            orders_included_before: vec![],
            orders_excluded_before: vec![],
            profit_before: U256::ZERO,
        },
    )?;
    Ok(ResultsWithoutExclusion {
        profit,
        orders_included,
        orders_failed,
    })
}

#[derive(Debug)]
struct ExclusionResults {
    landed_orders: HashMap<OrderId, ExclusionResult>,
    identities: HashMap<Address, ExclusionResult>,
    joint_exclusion_result: HashMap<(Address, Address), ExclusionResult>,
}

impl ExclusionResults {
    fn identity_exclusion(&self, address: &Address) -> &ExclusionResult {
        self.identities
            .get(address)
            .expect("Identity exclusion not found")
    }

    fn order_exclusion(&self, order_id: &OrderId) -> &ExclusionResult {
        self.landed_orders
            .get(order_id)
            .expect("Order exclusion not found")
    }

    fn joint_block_value_delta(&self) -> Vec<((Address, Address), I256)> {
        let mut result: Vec<_> = self
            .joint_exclusion_result
            .iter()
            .map(|((a1, a2), r)| ((*a1, *a2), r.block_value_delta))
            .collect();
        result.sort_by_key(|(a1, a2)| (*a1, *a2));
        result
    }
}

fn calculate_backtest_identity_and_order_exclusion<ConfigType: LiveBuilderConfig + Sync>(
    provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
    config: &ConfigType,
    block_data: BlockData,
    available_orders: &AvailableOrders,
    results_without_exclusion: &ResultsWithoutExclusion,
) -> eyre::Result<ExclusionResults> {
    let included_orders_exclusion = {
        let mut result = Vec::new();
        let mut included_orders_exclusions =
            available_orders.calc_individual_included_orders_exclusion();
        for order in &available_orders.included_orders_available {
            let id = order.order.id();
            let exclusions = included_orders_exclusions
                .remove(&id)
                .expect("included order exclusion not found");
            result.push((id, exclusions))
        }
        result
    };

    let result_after_landed_orders_exclusion: HashMap<OrderId, ExclusionResult> =
        included_orders_exclusion
            .into_par_iter()
            .map(|(id, exclusions)| {
                trace!(order = ?id, excluding = ?exclusions, "Excluding orders for landed order");
                calc_profit_after_exclusion(
                    provider_factory.clone(),
                    config,
                    &block_data,
                    results_without_exclusion.exclusion_input(exclusions),
                )
                .map(|ok| (id, ok))
            })
            .collect::<Result<_, _>>()?;

    let result_after_identity_exclusion: HashMap<Address, ExclusionResult> = available_orders
        .included_orders_by_address
        .to_vec()
        .into_par_iter()
        .map(|(address, _)| {
            let orders = available_orders
                .all_orders_by_address
                .get(&address)
                .expect("all orders by address not found")
                .clone();
            trace!(identity = ?address, excluding = ?orders, "Excluding orders for identity");
            calc_profit_after_exclusion(
                provider_factory.clone(),
                config,
                &block_data,
                results_without_exclusion.exclusion_input(orders),
            )
            .map(|ok| (address, ok))
        })
        .collect::<Result<_, _>>()?;

    Ok(ExclusionResults {
        landed_orders: result_after_landed_orders_exclusion,
        identities: result_after_identity_exclusion,
        joint_exclusion_result: HashMap::default(),
    })
}

fn calc_joint_exclusion_results<ConfigType: LiveBuilderConfig + Sync>(
    provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
    config: &ConfigType,
    block_data: BlockData,
    available_orders: &AvailableOrders,
    results_without_exclusion: &ResultsWithoutExclusion,
    mut exclusion_results: ExclusionResults,
) -> eyre::Result<ExclusionResults> {
    // calculate identities that are possibly connected
    let mut joint_contribution_todo: Vec<(Address, Address)> = Vec::new();
    // here we link identities if excluding order by one identity
    // leads to exclusion of order from another identitiy
    for (order, exclusion_result) in &exclusion_results.landed_orders {
        let address1 = *available_orders
            .orders_id_to_address
            .get(order)
            .expect("order address not found");
        let candidate_conflicting_bundles = exclusion_result
            .new_orders_failed
            .iter()
            .chain(
                exclusion_result
                    .orders_profit_changed
                    .iter()
                    .map(|(o, _)| o),
            )
            .filter(|o| !matches!(o, OrderId::Tx(_)));
        for new_failed_order in candidate_conflicting_bundles {
            // we only consider landed <-> landed order conflicts
            if !available_orders
                .included_orders_available
                .iter()
                .any(|o| o.order.id() == *new_failed_order)
            {
                continue;
            }
            let address2 = *available_orders
                .orders_id_to_address
                .get(new_failed_order)
                .expect("order address not found");
            if address1 != address2 {
                joint_contribution_todo.push((min(address1, address2), max(address1, address2)));
                warn!(address1 = ?address1, order1 = ?order, address2 = ?address2, order2 = ?new_failed_order, "Possible identity conflict");
            }
        }
    }
    joint_contribution_todo.sort();
    joint_contribution_todo.dedup();

    exclusion_results.joint_exclusion_result = joint_contribution_todo
        .into_par_iter()
        .map(|(address1, address2)| {
            let orders1 = available_orders
                .all_orders_by_address
                .get(&address1)
                .expect("orders by address not found")
                .clone();
            let orders2 = available_orders
                .all_orders_by_address
                .get(&address2)
                .expect("orders by address not found")
                .clone();
            let orders = orders1.iter().chain(orders2.iter()).cloned().collect();
            trace!(?address1, ?address2, excluding = ?orders, "Calculating joint contribution");
            calc_profit_after_exclusion(
                provider_factory.clone(),
                config,
                &block_data,
                results_without_exclusion.exclusion_input(orders),
            )
            .map(|ok| ((address1, address2), ok))
        })
        .collect::<Result<_, _>>()?;

    for ((address1, address2), result) in &exclusion_results.joint_exclusion_result {
        let block_value_delta = result.block_value_delta;
        if !block_value_delta.is_positive() {
            warn!(?address1, ?address2, newly_included_orders = ?result.new_orders_included, "Joint block value delta is not positive");
        };
        let bvd1 = exclusion_results
            .identity_exclusion(address1)
            .block_value_delta;
        let bvd2 = exclusion_results
            .identity_exclusion(address2)
            .block_value_delta;
        if bvd1 + bvd2 > block_value_delta {
            warn!(address1 = ?address1, address2 = ?address2, "Joint block value delta is smaller than sum of individual block value deltas");
        }
    }

    Ok(exclusion_results)
}

fn apply_redistribution_formula(
    onchain_block_profit: U256,
    available_orders: &AvailableOrders,
    exclusion_results: &ExclusionResults,
    restored_landed_orders: &HashMap<OrderId, LandedOrderData>,
) -> RedistributionResult {
    let mut identity_data = Vec::new();
    for (address, landed_orders) in &available_orders.included_orders_by_address {
        let mut included_orders = Vec::new();
        for id in landed_orders {
            let restored_landed_order = restored_landed_orders
                .get(id)
                .expect("order is not restored");
            if let Some(error) = &restored_landed_order.error {
                warn!(identity = ?address, order = ?id, err = ?error, "Landed order is not properly recovered");
                continue;
            }
            debug!(identity = ?address, order = ?id, "Landed order is properly recovered");

            let realized_value = restored_landed_order.unique_coinbase_profit;
            if !realized_value.is_positive() {
                warn!(identity = ?address, order = ?id, realized_value = format_ether(realized_value), "Order unique coinbase profit is not positive");
                continue;
            }
            let realized_value = realized_value.into_sign_and_abs().1;
            debug!(identity = ?address, order = ?id, realized_value = format_ether(realized_value), "Order unique coinbase profit");
            included_orders.push(IncludedOrderData {
                id: *id,
                realized_value,
            });
        }
        let block_value_delta = exclusion_results
            .identity_exclusion(address)
            .block_value_delta;
        if !block_value_delta.is_positive() {
            warn!(identity = ?address, block_value_delta = format_ether(block_value_delta), "Identity block value delta is not positive");
            continue;
        }
        let block_value_delta = block_value_delta.into_sign_and_abs().1;
        debug!(identity = ?address, block_value_delta = format_ether(block_value_delta), "Identity block value delta");
        identity_data.push(RedistributionIdentityData {
            address: *address,
            block_value_delta,
            included_orders,
        })
    }
    let joint_block_value_delta = exclusion_results
        .joint_block_value_delta()
        .into_iter()
        .filter_map(|((a1, a2), v)| {
            if v.is_positive() {
                Some(((a1, a2), v.into_sign_and_abs().1))
            } else {
                None
            }
        })
        .collect();

    RedistributionCalculator {
        landed_block_profit: onchain_block_profit,
        identity_data,
        joint_block_value_delta,
    }
    .calculate_redistribution()
}

fn collect_redistribution_result(
    block_data: &BlockData,
    onchain_block_profit: U256,
    available_orders: AvailableOrders,
    results_without_exclusion: ResultsWithoutExclusion,
    exclusion_results: ExclusionResults,
    calculated_redistribution_result: RedistributionResult,
    restored_landed_orders: &HashMap<OrderId, LandedOrderData>,
) -> RedistributionBlockOutput {
    let mut result = RedistributionBlockOutput {
        block_number: block_data.block_number,
        block_hash: block_data.onchain_block.header.hash,
        block_profit: onchain_block_profit,
        identities: Vec::new(),
        landed_orders: Vec::new(),
        joint_contribution: exclusion_results
            .joint_block_value_delta()
            .into_iter()
            .map(|((a1, a2), v)| JointContributionData {
                identities: vec![a1, a2],
                block_value_delta: v,
            })
            .collect(),
    };

    // let order_to_profit_without_exclusion = orders_included.
    for (address, landed_orders) in available_orders.included_orders_by_address {
        let redistribution_identity_data = calculated_redistribution_result
            .value_by_identity
            .iter()
            .find(|id| id.address == address)
            .cloned();
        for id in landed_orders {
            let redistribution_value_received = redistribution_identity_data
                .as_ref()
                .and_then(|d| {
                    d.order_redistribution_value
                        .iter()
                        .find(|(o, _)| o == &id)
                        .cloned()
                })
                .map(|(_, v)| v)
                .unwrap_or_default();
            let result_after_order_exclusion = exclusion_results.order_exclusion(&id);
            let block_value_delta = result_after_order_exclusion.block_value_delta;
            let restored_order_data = restored_landed_orders
                .get(&id)
                .expect("restored landed order is not found");
            let realized_value = restored_order_data
                .unique_coinbase_profit
                .try_into()
                .unwrap_or_default();
            info!(identity = ?address,
                order = ?id,
                block_value_delta = format_ether(block_value_delta),
                redistribution_value_received = format_ether(redistribution_value_received),
                realized_value = format_ether(realized_value),
                "Included order data"
            );
            result.landed_orders.push(OrderData {
                id: ExtendedOrderId::new(id, &available_orders.bundle_hash_by_id),
                identity: address,
                sender: available_orders
                    .order_sender_by_id
                    .get(&id)
                    .cloned()
                    .unwrap_or_default(),
                redistribution_value_received,
                block_value_delta,
                inclusion_changes: calc_inclusion_change(
                    &available_orders.bundle_hash_by_id,
                    result_after_order_exclusion,
                    &results_without_exclusion.orders_included,
                ),
                realized_value,
            })
        }
        let result_after_identity_exclusion = exclusion_results.identity_exclusion(&address);
        let block_value_delta = result_after_identity_exclusion.block_value_delta;
        let redistribution_value_received = redistribution_identity_data
            .map(|d| d.redistribution_value)
            .unwrap_or_default();
        info!(identity = ?address,
                block_value_delta = format_ether(block_value_delta),
                redistribution_value_received = format_ether(redistribution_value_received),
                "Identity data"
        );
        result.identities.push(IdentityData {
            address,
            redistribution_value_received,
            block_value_delta,
            inclusion_changes: calc_inclusion_change(
                &available_orders.bundle_hash_by_id,
                result_after_identity_exclusion,
                &results_without_exclusion.orders_included,
            ),
        })
    }
    result
}

fn calc_inclusion_change(
    bundle_hash_by_id: &HashMap<OrderId, B256>,
    exclusion_result: &ExclusionResult,
    included_before: &[(OrderId, U256)],
) -> Vec<OrderInclusionChange> {
    let mut result = Vec::new();
    for (id, profit_after) in &exclusion_result.new_orders_included {
        result.push((
            *id,
            OrderInclusionChange {
                id: ExtendedOrderId::new(*id, bundle_hash_by_id),
                change: InclusionChange::Included,
                profit_before: U256::ZERO,
                profit_after: *profit_after,
            },
        ));
    }
    for id in &exclusion_result.new_orders_failed {
        let profit_before = included_before
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, p)| *p)
            .unwrap_or_default();
        result.push((
            *id,
            OrderInclusionChange {
                id: ExtendedOrderId::new(*id, bundle_hash_by_id),
                change: InclusionChange::Excluded,
                profit_before,
                profit_after: U256::ZERO,
            },
        ));
    }
    for (id, profit_after) in &exclusion_result.orders_profit_changed {
        let profit_before = included_before
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, p)| *p)
            .unwrap_or_default();
        result.push((
            *id,
            OrderInclusionChange {
                id: ExtendedOrderId::new(*id, bundle_hash_by_id),
                change: InclusionChange::ProfitChanged,
                profit_before,
                profit_after: *profit_after,
            },
        ))
    }
    result.sort_by_key(|(id, _)| *id);
    result.into_iter().map(|(_, v)| v).collect()
}

#[derive(Debug)]
struct ExclusionInput {
    exclude_orders: Vec<OrderId>,
    orders_included_before: Vec<(OrderId, U256)>,
    orders_excluded_before: Vec<OrderId>,
    profit_before: U256,
}

#[derive(Debug)]
struct ExclusionResult {
    profit: U256,
    block_value_delta: I256,
    new_orders_failed: Vec<OrderId>,
    new_orders_included: Vec<(OrderId, U256)>,
    orders_profit_changed: Vec<(OrderId, U256)>,
}

/// calculate block profit excluding some orders
fn calc_profit_after_exclusion<ConfigType: LiveBuilderConfig>(
    provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
    config: &ConfigType,
    block_data: &BlockData,
    exclusion_input: ExclusionInput,
) -> eyre::Result<ExclusionResult> {
    let block_data_with_excluded = {
        let mut block_data = block_data.clone();
        block_data
            .available_orders
            .retain(|order| !exclusion_input.exclude_orders.contains(&order.order.id()));
        block_data
    };

    let available_orders: HashSet<_> = block_data_with_excluded
        .available_orders
        .iter()
        .map(|order| order.order.id())
        .collect();
    let orders_included_before: HashMap<_, _> =
        exclusion_input.orders_included_before.into_iter().collect();
    let orders_excluded_before: HashSet<_> =
        exclusion_input.orders_excluded_before.into_iter().collect();

    let base_config = config.base_config();

    // we set built_block_lag_ms to 0 here because we already prefiltered all the orders
    // in built_block_data, so we essentially just disable filtering in the `backtest_simulate_block`
    // but we still filter by the relay timestamp
    let built_block_lag_ms = 0;

    let result = backtest_simulate_block(
        block_data_with_excluded,
        provider_factory.clone(),
        base_config.chain_spec()?,
        built_block_lag_ms,
        base_config.backtest_builders.clone(),
        config,
        base_config.blocklist()?,
        &base_config.sbundle_mergeabe_signers(),
    )?
    .builder_outputs
    .into_iter()
    .max_by_key(|o| o.our_bid_value)
    .ok_or(eyre::eyre!("No max value found"))?;

    let orders_included: HashMap<_, _> = result
        .included_orders
        .into_iter()
        .zip(result.included_order_profits)
        .collect();

    let mut new_orders_included = Vec::new();
    let mut orders_profit_changed = Vec::new();
    for (inc_ord, inc_profit) in &orders_included {
        if !orders_included_before.contains_key(inc_ord) {
            new_orders_included.push((*inc_ord, *inc_profit));
        } else if orders_included_before.get(inc_ord) != Some(inc_profit) {
            orders_profit_changed.push((*inc_ord, *inc_profit));
        }
    }

    let mut new_orders_failed = Vec::new();
    for order in available_orders {
        if !orders_included.contains_key(&order) && !orders_excluded_before.contains(&order) {
            new_orders_failed.push(order);
        }
    }

    let block_value_delta = signed_uint_delta(exclusion_input.profit_before, result.our_bid_value);

    Ok(ExclusionResult {
        profit: result.our_bid_value,
        block_value_delta,
        new_orders_failed,
        new_orders_included,
        orders_profit_changed,
    })
}

fn order_redistribution_address(order: &Order, protect_signers: &[Address]) -> Option<Address> {
    let signer = match order.signer() {
        Some(signer) => signer,
        None => {
            return if order.is_tx() {
                Some(order.list_txs().first()?.0.signer())
            } else {
                None
            }
        }
    };

    if !protect_signers.contains(&signer) {
        return Some(signer);
    }

    match order {
        Order::Bundle(bundle) => {
            // if its just a bundle we take origin tx of the first transaction
            let tx = bundle.txs.first()?;
            Some(tx.signer())
        }
        Order::ShareBundle(bundle) => {
            // if it is a share bundle we take either
            // 1. first address from the refund config
            // 2. origin of the first tx

            if let Some(first_refund) = bundle.inner_bundle.refund_config.first() {
                return Some(first_refund.address);
            }

            let txs = bundle.list_txs();
            let (first_tx, _) = txs.first()?;
            Some(first_tx.signer())
        }
        Order::Tx(_) => {
            unreachable!("Mempool tx order can't have signer");
        }
    }
}

fn order_sender(order: &Order) -> Address {
    let mut txs = order.list_txs();
    if txs.len() == 1 {
        return txs.pop().unwrap().0.signer();
    }

    order.signer().unwrap_or_default()
}
