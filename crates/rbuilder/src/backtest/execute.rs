use crate::{
    backtest::BlockData,
    building::{
        builders::BacktestSimulateBlockInput, multi_share_bundle_merger::MultiShareBundleMerger,
        sim::simulate_all_orders_with_sim_tree, BlockBuildingContext, BundleErr, OrderErr,
        SimulatedOrderSink, SimulatedOrderStore, TransactionErr,
    },
    live_builder::cli::LiveBuilderConfig,
    primitives::{OrderId, SimulatedOrder},
    provider::StateProviderFactory,
    utils::{clean_extradata, Signer},
};
use ahash::HashSet;
use alloy_primitives::{Address, U256};
use reth::revm::cached::CachedReads;
use reth_chainspec::ChainSpec;
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, rc::Rc, sync::Arc};

use super::OrdersWithTimestamp;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BacktestBuilderOutput {
    pub orders_included: usize,
    pub builder_name: String,
    pub our_bid_value: U256,
    #[serde(default)]
    pub included_orders: Vec<OrderId>,
    #[serde(default)]
    pub included_order_profits: Vec<U256>,
}

/// Result of a backtest simulation usually stored for later comparison
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockBacktestValue {
    pub block_number: u64,
    /// mev boost bid trace value of the winning bid
    pub winning_bid_value: U256,
    /// orders that were available for block building after simulations
    pub simulated_orders_count: usize,
    /// sum of gas spent by simulated orders
    pub simulated_total_gas: u64,
    /// number of orders filtered because of blocklist
    pub filtered_orders_blocklist_count: usize,
    /// number of simulated orders with kickback txs
    pub simulated_orders_with_refund: usize,
    /// sum of eth kickbacks paid by simulated orders
    pub simulated_refunds_paid: U256,
    pub extra_data: String,
    pub builder_outputs: Vec<BacktestBuilderOutput>,
}

#[derive(Debug)]
pub struct BacktestBlockInput {
    pub ctx: BlockBuildingContext,
    pub sim_orders: Vec<SimulatedOrder>,
    pub sim_errors: Vec<OrderErr>,
}

pub fn backtest_prepare_ctx_for_block<P>(
    block_data: BlockData,
    provider: P,
    chain_spec: Arc<ChainSpec>,
    blocklist: HashSet<Address>,
    sbundle_mergeabe_signers: &[Address],
    builder_signer: Signer,
) -> eyre::Result<BacktestBlockInput>
where
    P: StateProviderFactory + Clone + 'static,
{
    let ctx = BlockBuildingContext::from_onchain_block(
        block_data.onchain_block,
        chain_spec.clone(),
        None,
        blocklist,
        builder_signer.address,
        block_data.winning_bid_trace.proposer_fee_recipient,
        Some(builder_signer),
        Arc::from(provider.root_hasher(block_data.winning_bid_trace.parent_hash)?),
    );
    backtest_prepare_ctx_for_block_from_building_context(
        ctx,
        block_data.available_orders,
        provider,
        sbundle_mergeabe_signers,
    )
}

pub fn backtest_prepare_ctx_for_block_from_building_context<P>(
    ctx: BlockBuildingContext,
    available_orders: Vec<OrdersWithTimestamp>,
    provider: P,
    sbundle_mergeabe_signers: &[Address],
) -> eyre::Result<BacktestBlockInput>
where
    P: StateProviderFactory + Clone + 'static,
{
    let orders = available_orders
        .iter()
        .map(|order| order.order.clone())
        .collect::<Vec<_>>();

    let (sim_orders, sim_errors) =
        simulate_all_orders_with_sim_tree(provider, &ctx, &orders, false)?;

    // Apply bundle merging as in live building.
    let order_store = Rc::new(RefCell::new(SimulatedOrderStore::new()));
    let mut merger = MultiShareBundleMerger::new(sbundle_mergeabe_signers, order_store.clone());
    for sim_order in sim_orders {
        merger.insert_order(sim_order);
    }
    let sim_orders = order_store.borrow().get_orders();
    Ok(BacktestBlockInput {
        ctx,
        sim_orders,
        sim_errors,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn backtest_simulate_block<P, ConfigType>(
    block_data: BlockData,
    provider: P,
    chain_spec: Arc<ChainSpec>,
    builders_names: Vec<String>,
    config: &ConfigType,
    blocklist: HashSet<Address>,
    sbundle_mergeabe_signers: &[Address],
) -> eyre::Result<BlockBacktestValue>
where
    P: StateProviderFactory + Clone + 'static,
    ConfigType: LiveBuilderConfig,
{
    let BacktestBlockInput {
        ctx,
        sim_orders,
        sim_errors,
    } = backtest_prepare_ctx_for_block(
        block_data.clone(),
        provider.clone(),
        chain_spec.clone(),
        blocklist,
        sbundle_mergeabe_signers,
        config.base_config().coinbase_signer()?,
    )?;

    let filtered_orders_blocklist_count = sim_errors
        .into_iter()
        .filter(|err| {
            matches!(
                err,
                OrderErr::Transaction(TransactionErr::Blocklist)
                    | OrderErr::Bundle(BundleErr::InvalidTransaction(_, TransactionErr::Blocklist))
            )
        })
        .count();

    let (simulated_orders_with_refund, simulated_refunds_paid) = {
        let mut count = 0;
        let mut amount = U256::ZERO;
        for sim in &sim_orders {
            if sim.sim_value.paid_kickbacks.is_empty() {
                continue;
            }
            count += 1;
            amount += sim
                .sim_value
                .paid_kickbacks
                .iter()
                .map(|(_, v)| v)
                .sum::<U256>();
        }
        (count, amount)
    };

    let simulated_total_gas = sim_orders.iter().map(|o| o.sim_value.gas_used).sum();
    let mut builder_outputs = Vec::new();

    let mut cached_reads = Some(CachedReads::default());
    for building_algorithm_name in builders_names {
        let input = BacktestSimulateBlockInput {
            ctx: ctx.clone(),
            builder_name: building_algorithm_name.clone(),
            sim_orders: &sim_orders,
            provider: provider.clone(),
            cached_reads,
        };

        let (block, new_cached_reads) =
            config.build_backtest_block(&building_algorithm_name, input)?;
        cached_reads = Some(new_cached_reads);
        builder_outputs.push(BacktestBuilderOutput {
            orders_included: block.trace.included_orders.len(),
            builder_name: building_algorithm_name,
            our_bid_value: block.trace.bid_value,
            included_orders: block
                .trace
                .included_orders
                .iter()
                .map(|o| o.order.id())
                .collect(),
            included_order_profits: block
                .trace
                .included_orders
                .iter()
                .map(|o| o.coinbase_profit)
                .collect(),
        });
    }

    Ok(BlockBacktestValue {
        block_number: block_data.block_number,
        winning_bid_value: block_data.winning_bid_trace.value,
        simulated_orders_count: sim_orders.len(),
        simulated_total_gas,
        filtered_orders_blocklist_count,
        simulated_orders_with_refund,
        simulated_refunds_paid,
        extra_data: clean_extradata(&block_data.onchain_block.header.extra_data),
        builder_outputs,
    })
}
