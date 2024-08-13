mod cli;
mod redistribution_algo;

use crate::backtest::execute::backtest_simulate_block;
use crate::backtest::redistribute::redistribution_algo::{
    calculate_redistribution, IncludedOrderData, RedistributionCalculator,
    RedistributionIdentityData,
};
use crate::backtest::restore_landed_orders::{
    restore_landed_orders, sim_historical_block, ExecutedBlockTx, SimplifiedOrder,
};
use crate::backtest::BlockData;
use crate::live_builder::cli::LiveBuilderConfig;
use crate::primitives::{Order, OrderId};
use crate::utils::signed_uint_delta;
use ahash::HashMap;
use alloy_primitives::utils::format_ether;
use alloy_primitives::{Address, U256};
pub use cli::run_backtest_redistribute;
use rayon::prelude::*;
use reth_db::DatabaseEnv;
use reth_provider::ProviderFactory;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, info_span, trace, warn};

pub fn calc_redistributions<ConfigType: LiveBuilderConfig + Send + Sync>(
    provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
    config: &ConfigType,
    mut block_data: BlockData,
    distribute_to_mempool_txs: bool,
) -> eyre::Result<Vec<(Address, U256)>> {
    let _block_span = info_span!("block", block = block_data.block_number).entered();
    let built_block_data = if let Some(block_data) = block_data.built_block_data.clone() {
        block_data
    } else {
        warn!(block = block_data.block_number, "Block data not found");
        return Ok(Vec::new());
    };

    let protect_signers = config.base_config().backtest_protect_bundle_signers.clone();

    let block_profit = if built_block_data.profit.is_positive() {
        built_block_data.profit.into_sign_and_abs().1
    } else {
        // there is nothing to distribute
        return Ok(Vec::new());
    };

    block_data.filter_orders_by_end_timestamp(built_block_data.orders_closed_at);
    // @TODO filter cancellations properly, for this we need actual cancellations in the backtest data
    // filter bundles made out of mempool txs
    block_data.filter_bundles_from_mempool();

    let orders_by_id = block_data
        .available_orders
        .iter()
        .map(|order| (order.order.id(), order.clone()))
        .collect::<HashMap<_, _>>();

    let mut landed_orders_by_address: HashMap<Address, Vec<OrderId>> = HashMap::default();
    let mut all_orders_by_address: HashMap<Address, Vec<OrderId>> = HashMap::default();

    // detect landed orders in the onchain block
    let restored_landed_orders = {
        let block_txs = sim_historical_block(
            provider_factory.clone(),
            config.base_config().chain_spec()?,
            block_data.onchain_block.clone(),
        )?
        .into_iter()
        .map(|executed_tx| {
            ExecutedBlockTx::new(
                executed_tx.tx.hash,
                executed_tx.coinbase_profit,
                executed_tx.receipt.success,
            )
        })
        .collect::<Vec<_>>();

        for o in &block_data.available_orders {
            if o.order.is_tx() && !distribute_to_mempool_txs {
                continue;
            }

            let address = match order_redistribution_address(&o.order, &protect_signers) {
                Some(address) => address,
                None => {
                    warn!(order = ?o.order.id(), "Order redistribution address not found");
                    continue;
                }
            };
            if o.order.is_tx() && distribute_to_mempool_txs {
                let tx_hash = o.order.id().tx_hash().expect("order is tx");
                if block_txs.iter().any(|tx| tx.hash == tx_hash) {
                    landed_orders_by_address
                        .entry(address)
                        .or_default()
                        .push(o.order.id());
                }
            }
            all_orders_by_address
                .entry(address)
                .or_default()
                .push(o.order.id());
        }

        let mut simplified_orders = Vec::new();

        for included_order in &built_block_data.included_orders {
            match orders_by_id.get(included_order) {
                Some(order) => {
                    let address = match order_redistribution_address(&order.order, &protect_signers)
                    {
                        Some(address) => address,
                        None => {
                            warn!(order = ?order, "Order redistribution address not found");
                            continue;
                        }
                    };
                    landed_orders_by_address
                        .entry(address)
                        .or_default()
                        .push(order.order.id());
                    simplified_orders.push(SimplifiedOrder::new_from_order(&order.order));
                }
                None => {
                    warn!(order = ?included_order, "Included order not found in available orders");
                }
            }
        }

        // we include mempool txs into the set of simplified orders
        // so we correctly subtract contributions of mempool txs
        for available_order in &block_data.available_orders {
            if available_order.order.is_tx() {
                simplified_orders.push(SimplifiedOrder::new_from_order(&available_order.order));
            }
        }

        restore_landed_orders(block_txs, simplified_orders)
    };

    let start = Instant::now();
    let profit_without_exclusion = calc_profit(provider_factory.clone(), config, &block_data, &[])?;

    let profit_after_address_exclusion = all_orders_by_address
        .into_iter()
        .filter(|(address, _)| landed_orders_by_address.contains_key(address))
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|(address, orders)| {
            (
                address,
                calc_profit(provider_factory.clone(), config, &block_data, &orders),
            )
        })
        .collect::<HashMap<_, _>>();
    debug!(
        elapsed_ms = start.elapsed().as_millis(),
        "Calculated address contribution"
    );

    // sort for deterministic output
    let mut orders_by_address = landed_orders_by_address.into_iter().collect::<Vec<_>>();
    orders_by_address.sort_by_key(|(a, _)| *a);
    for (_, orders) in &mut orders_by_address {
        orders.sort();
    }

    // prepare input for the redistribution algo
    let mut redistribution_calc = RedistributionCalculator {
        landed_block_profit: block_profit,
        identity_data: vec![],
    };

    for (address, orders) in orders_by_address {
        let _span_guard = info_span!("redistribution", ?address).entered();
        let profit_after_exclusion = match profit_after_address_exclusion.get(&address) {
            Some(Ok(profit)) => *profit,
            Some(Err(err)) => {
                warn!(?err, "Profit after exclusion calculation failed");
                continue;
            }
            None => {
                warn!("Profit after exclusion not found");
                continue;
            }
        };
        let value_delta_after_exclusion =
            signed_uint_delta(profit_without_exclusion, profit_after_exclusion);

        trace!(
            delta = format_ether(value_delta_after_exclusion),
            "Value delta after identity exclusion."
        );

        let mut included_orders = Vec::new();
        for order in orders {
            let _span_guard = info_span!("order", ?order).entered();
            let landed_order_data = match restored_landed_orders.get(&order) {
                Some(landed_order_data) => landed_order_data.clone(),
                None => {
                    warn!("Landed order data not found");
                    continue;
                }
            };
            trace!(err = ?landed_order_data.error, unique_coinbase_profit = format_ether(landed_order_data.unique_coinbase_profit), "Landed order");
            if landed_order_data.error.is_some() {
                warn!(err = ?landed_order_data.error, "Landed order identification failed")
            }
            included_orders.push(IncludedOrderData {
                id: order,
                landed_order_data,
            });
        }

        redistribution_calc
            .identity_data
            .push(RedistributionIdentityData {
                address,
                value_delta_after_exclusion,
                included_orders,
            });
    }

    let result = calculate_redistribution(redistribution_calc);
    info!(
        landed_block_profit = format_ether(result.landed_block_profit),
        total_value_redistributed = format_ether(result.total_value_redistributed),
        "Redistribution calculated"
    );
    let mut output = Vec::new();
    for identity in result.value_by_identity {
        let _id_span = info_span!("identity", address = ?identity.address).entered();
        info!(
            value = format_ether(identity.value_received),
            "Redistribution value"
        );
        for (order, contribution) in identity.order_contributions {
            info!(order = ?order, contribution = format_ether(contribution), "Order contribution");
        }
        if !identity.value_received.is_zero() {
            output.push((identity.address, identity.value_received));
        }
    }
    Ok(output)
}

/// calculate block profit excluding some orders
fn calc_profit<ConfigType: LiveBuilderConfig>(
    provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
    config: &ConfigType,
    block_data: &BlockData,
    exclude_orders: &[OrderId],
) -> eyre::Result<U256> {
    let block_data_with_excluded = {
        let mut block_data = block_data.clone();
        block_data
            .available_orders
            .retain(|order| !exclude_orders.contains(&order.order.id()));
        block_data
    };

    let base_config = config.base_config();

    let result = backtest_simulate_block(
        block_data_with_excluded,
        provider_factory.clone(),
        base_config.chain_spec()?,
        0,
        base_config.backtest_builders.clone(),
        config,
        base_config.blocklist()?,
        &base_config.sbundle_mergeabe_signers(),
    )?;
    Ok(result
        .builder_outputs
        .iter()
        .map(|o| o.our_bid_value)
        .max()
        .unwrap_or_default())
}

fn order_redistribution_address(order: &Order, protect_signers: &[Address]) -> Option<Address> {
    let signer = match order.signer() {
        Some(signer) => signer,
        None => {
            return if order.is_tx() {
                Some(order.list_txs().first()?.0.tx.signer())
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
            Some(tx.tx.signer())
        }
        Order::ShareBundle(bundle) => {
            // if it is a share bundle we take either
            // 1. last address from the refund config
            // 2. origin of the first tx

            if let Some(last_refund_config) = bundle.inner_bundle.refund_config.last() {
                return Some(last_refund_config.address);
            }

            let txs = bundle.list_txs();
            let (first_tx, _) = txs.first()?;
            Some(first_tx.tx.signer())
        }
        Order::Tx(_) => {
            unreachable!("Mempool tx order can't have signer");
        }
    }
}
