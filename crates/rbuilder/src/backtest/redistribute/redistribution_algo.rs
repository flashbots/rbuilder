todo!();

use alloy_primitives::{Address, I256, U256};
use tracing::info;
use crate::backtest::restore_landed_bundles::LandedOrderData;
use crate::primitives::OrderId;


struct IncludedOrderData {
    id: OrderId,
    valued_delta_after_exclusion: I256,
    landed_order_data: LandedOrderData,
    orders_excluded_by_this_order: Vec<OrderId>,
}

struct RedistributionIdentityData {
    address: Address,
    value_delta_after_exclusion: I256,
    included_orders: Vec<IncludedOrderData>,
}

struct RedistributionCalculator {
    landed_block_profit: U256,
    identity_data: Vec<RedistributionIdentityData>,
}


struct RedistributionEntityResult {
    address: Address,
    value_received: U256,
    bundle_contribution: Vec<(OrderId, U256)>
}

struct RedistributionResult {
    total_value_redistributed: U256,
    value_by_identity: Vec<RedistributionEntityResult>
}


fn calculate_redistribution(data: RedistributionCalculator) -> RedistributionResult {
    info!("Calculating redistribution");

    for identity in data.identity_data {
        if !identity.value_delta_after_exclusion.is_positive() {
            continue;
        }

        let mut total_value_paid_to_proposer = I256::ZERO;
        for included_order in identity.included_orders {
            total_value_paid_to_proposer += included_order.landed_order_data.unique_coinbase_profit;
        }
    }

    todo!()
}
