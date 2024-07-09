mod cli;

use crate::backtest::execute::backtest_simulate_block;
use crate::backtest::BlockData;
use crate::live_builder::cli::LiveBuilderConfig;
use crate::primitives::{Order, OrderId};
use ahash::{HashMap, HashSet};
use alloy_primitives::utils::format_ether;
use alloy_primitives::{Address, U256};
use reth_db::DatabaseEnv;
use reth_provider::ProviderFactory;
use std::sync::Arc;
use tracing::{info, warn};

pub use cli::run_backtest_redistribute;

pub fn calc_redistributions<ConfigType: LiveBuilderConfig>(
    provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
    config: &ConfigType,
    mut block_data: BlockData,
) -> eyre::Result<Vec<(Address, U256)>> {
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
    // @TODO filter cancellations properly

    let orders_by_id = block_data
        .available_orders
        .iter()
        .map(|order| (order.order.id(), order.clone()))
        .collect::<HashMap<_, _>>();

    let mut included_redistribution_addresses = HashSet::default();
    for order in &built_block_data.included_orders {
        if let Some(order) = orders_by_id.get(order) {
            let redistribution_address =
                order_redistribution_address(&order.order, &protect_signers);
            match redistribution_address {
                Some(address) => {
                    included_redistribution_addresses.insert(address);
                }
                None => {
                    warn!(?order, "Included order missing redistribution address");
                }
            }
        } else {
            warn!(?order, "Included order not found in available orders");
        }
    }

    let mut orders_by_address = HashMap::default();
    for order in &block_data.available_orders {
        if let Some(address) = order_redistribution_address(&order.order, &protect_signers) {
            orders_by_address
                .entry(address)
                .or_insert_with(Vec::new)
                .push(order.order.id());
        }
    }

    // calculate profit without any address exclusion
    let original_profit = calc_profit(provider_factory.clone(), config, &block_data, &[])?;

    info!(
        simulated_value = format_ether(original_profit),
        real_bid_value = format_ether(block_data.winning_bid_trace.value),
        "Simulated value of the block"
    );

    let mut included_addresses_contributions = Vec::new();

    for (address, order_ids) in orders_by_address.iter() {
        if !included_redistribution_addresses.contains(address) {
            continue;
        }

        let profit_after_exclusion =
            calc_profit(provider_factory.clone(), config, &block_data, order_ids)?;

        let excess_value = if profit_after_exclusion < original_profit {
            original_profit - profit_after_exclusion
        } else {
            // contribution of this address is 0
            continue;
        };

        info!(
            ?address,
            excess_value = format_ether(excess_value),
            "Contribution of the address"
        );

        included_addresses_contributions.push((*address, excess_value));
    }

    let total_contributions = included_addresses_contributions
        .iter()
        .map(|(_, v)| v)
        .sum::<U256>();

    let mut profit_shared_with_address = Vec::new();

    for (address, contribution) in included_addresses_contributions {
        let share = (contribution * block_profit) / total_contributions;
        profit_shared_with_address.push((address, share));
    }

    for (_, profit_share) in &profit_shared_with_address {
        if profit_share > &block_profit {
            eyre::bail!("Profit share exceeds block profit");
        }
    }

    let total_profits_shared = profit_shared_with_address
        .iter()
        .map(|(_, v)| v)
        .sum::<U256>();
    if total_profits_shared > block_profit {
        eyre::bail!("Total profits shared exceeds block profit");
    }

    Ok(profit_shared_with_address)
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
    let signer = order.signer()?;

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
