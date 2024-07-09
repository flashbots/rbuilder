mod cli;

use crate::backtest::execute::backtest_simulate_block;
use crate::backtest::BlockData;
use crate::live_builder::cli::LiveBuilderConfig;
use crate::primitives::OrderId;
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

    let mut included_signers = HashSet::default();
    for order in &built_block_data.included_orders {
        if let Some(order) = orders_by_id.get(order) {
            match order.order.signer() {
                Some(signer) => {
                    included_signers.insert(signer);
                }
                None => {
                    warn!(?order, "Included order missing signer");
                }
            }
        } else {
            warn!(?order, "Included order not found in available ordrers");
        }
    }

    let mut orders_by_signer = HashMap::default();
    for order in &block_data.available_orders {
        if let Some(signer) = order.order.signer() {
            orders_by_signer
                .entry(signer)
                .or_insert_with(Vec::new)
                .push(order.order.id());
        }
    }

    // calculate profit without any signer exclusion
    let original_profit = calc_profit(provider_factory.clone(), config, &block_data, &[])?;

    let mut included_signers_contributions = Vec::new();

    for (signer, order_ids) in orders_by_signer.iter() {
        if !included_signers.contains(signer) {
            continue;
        }

        let profit_after_exclusion =
            calc_profit(provider_factory.clone(), config, &block_data, order_ids)?;

        let excess_value = if profit_after_exclusion < original_profit {
            original_profit - profit_after_exclusion
        } else {
            // contribution of this signer is 0
            continue;
        };

        info!(
            ?signer,
            excess_value = format_ether(excess_value),
            "Contribution of the signer"
        );

        included_signers_contributions.push((*signer, excess_value));
    }

    let total_signers_contributions = included_signers_contributions
        .iter()
        .map(|(_, v)| v)
        .sum::<U256>();

    let mut profit_shared_with_signers = Vec::new();

    for (signer, contribution) in included_signers_contributions {
        let share = (contribution * block_profit) / total_signers_contributions;
        profit_shared_with_signers.push((signer, share));
    }

    for (_, profit_share) in &profit_shared_with_signers {
        if profit_share > &block_profit {
            eyre::bail!("Profit share exceeds block profit");
        }
    }

    let total_profits_shared = profit_shared_with_signers
        .iter()
        .map(|(_, v)| v)
        .sum::<U256>();
    if total_profits_shared > block_profit {
        eyre::bail!("Total profits shared exceeds block profit");
    }

    Ok(profit_shared_with_signers)
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
