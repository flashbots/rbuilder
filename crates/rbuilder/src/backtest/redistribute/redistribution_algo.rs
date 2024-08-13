use crate::backtest::restore_landed_orders::LandedOrderData;
use crate::primitives::OrderId;
use alloy_primitives::{Address, I256, U256};
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub struct IncludedOrderData {
    pub id: OrderId,
    pub landed_order_data: LandedOrderData,
}

#[derive(Debug)]
pub struct RedistributionIdentityData {
    pub address: Address,
    /// This is the difference between block value with all orders and
    /// block value when we exclude orders by this identity
    pub value_delta_after_exclusion: I256,
    pub included_orders: Vec<IncludedOrderData>,
}

#[derive(Debug)]
pub struct RedistributionCalculator {
    pub landed_block_profit: U256,
    pub identity_data: Vec<RedistributionIdentityData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct RedistributionEntityResult {
    pub address: Address,
    pub value_received: U256,
    /// Contribution to the value_received by each individual order
    pub order_contributions: Vec<(OrderId, U256)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct RedistributionResult {
    /// This is onchain block profit that is being redistributed
    pub landed_block_profit: U256,
    /// Total value that is redistributed to different identities <= landed_block_profit
    pub total_value_redistributed: U256,
    pub value_by_identity: Vec<RedistributionEntityResult>,
}

pub fn calculate_redistribution(data: RedistributionCalculator) -> RedistributionResult {
    let mut value_after_exclusion = Vec::new();
    let mut value_paid_to_coinbase = Vec::new();
    let mut identity_orders = Vec::new();
    let mut value_paid_by_order_to_coinbase = Vec::new();
    for identity in &data.identity_data {
        let (sign, abs) = identity.value_delta_after_exclusion.into_sign_and_abs();
        if sign.is_positive() {
            value_after_exclusion.push(abs);
        } else {
            value_after_exclusion.push(U256::ZERO);
        }
        let mut order_paid_to_coinbase = Vec::new();
        let mut order_id = Vec::new();
        for order in &identity.included_orders {
            let (sign, abs) = order
                .landed_order_data
                .unique_coinbase_profit
                .into_sign_and_abs();
            order_id.push(order.id);
            if sign.is_positive() {
                order_paid_to_coinbase.push(abs);
            } else {
                order_paid_to_coinbase.push(U256::ZERO);
            }
        }
        value_paid_to_coinbase.push(order_paid_to_coinbase.iter().sum::<U256>());
        value_paid_by_order_to_coinbase.push(order_paid_to_coinbase);
        identity_orders.push(order_id);
    }

    // we clamp value after exclusion to the value paid to coinbase
    // so that orders that had huge contribution in backtest but small onchain will not suppress others
    for i in 0..value_after_exclusion.len() {
        value_after_exclusion[i] =
            std::cmp::min(value_after_exclusion[i], value_paid_to_coinbase[i]);
    }

    let mut result = RedistributionResult {
        landed_block_profit: data.landed_block_profit,
        total_value_redistributed: U256::ZERO,
        value_by_identity: vec![],
    };

    let total_value_split = split_value(data.landed_block_profit, &value_after_exclusion);
    for (i, identity) in data.identity_data.into_iter().enumerate() {
        let value_received = std::cmp::min(total_value_split[i], value_paid_to_coinbase[i]);
        result.total_value_redistributed += value_received;
        let per_order_split = split_value(value_received, &value_paid_by_order_to_coinbase[i]);
        let order_contribution = identity_orders[i]
            .iter()
            .cloned()
            .zip(per_order_split.into_iter())
            .collect();
        result.value_by_identity.push(RedistributionEntityResult {
            address: identity.address,
            value_received,
            order_contributions: order_contribution,
        })
    }

    result
}

fn split_value(value: U256, split_vector: &[U256]) -> Vec<U256> {
    let total_split = split_vector.iter().sum::<U256>();
    if total_split.is_zero() {
        return split_vector.iter().map(|_| U256::ZERO).collect();
    }
    let mut result = Vec::new();
    for split in split_vector {
        result.push((value * split) / total_split);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::test_utils::*;

    #[test]
    fn test_split_value() {
        let test_data = vec![
            (100, vec![100], vec![100]),
            (100, vec![30, 70], vec![30, 70]),
            (100, vec![10, 20], vec![33, 66]),
            (100, vec![100, 200], vec![33, 66]),
            (100, vec![0, 0], vec![0, 0]),
        ];

        for (value, split_vector, expected) in test_data {
            let value = U256::from(value);
            let split_vector = split_vector
                .into_iter()
                .map(U256::from)
                .collect::<Vec<U256>>();
            let expected = expected.into_iter().map(U256::from).collect::<Vec<U256>>();
            assert_eq!(split_value(value, &split_vector), expected);
        }
    }

    #[test]
    fn test_calculate_redistribution() {
        let input = RedistributionCalculator {
            landed_block_profit: u256(1000),
            identity_data: vec![
                // simple identity with one bundle
                RedistributionIdentityData {
                    address: addr(1),
                    value_delta_after_exclusion: i256(10),
                    included_orders: vec![IncludedOrderData {
                        id: order_id(0x11),
                        landed_order_data: LandedOrderData {
                            order: order_id(0x11),
                            unique_coinbase_profit: i256(10),
                            total_coinbase_profit: i256(10),
                            error: None,
                            overlapping_txs: vec![],
                        },
                    }],
                },
                // identity value after exclusion is negative
                RedistributionIdentityData {
                    address: addr(2),
                    value_delta_after_exclusion: i256(-10),
                    included_orders: vec![IncludedOrderData {
                        id: order_id(0x21),
                        landed_order_data: LandedOrderData {
                            order: order_id(0x21),
                            unique_coinbase_profit: i256(-10),
                            total_coinbase_profit: i256(-10),
                            error: None,
                            overlapping_txs: vec![],
                        },
                    }],
                },
                // identity has no landed bundles
                RedistributionIdentityData {
                    address: addr(3),
                    value_delta_after_exclusion: i256(10),
                    included_orders: vec![],
                },
                // identity has bundle with negative contribution, bundle
                RedistributionIdentityData {
                    address: addr(4),
                    value_delta_after_exclusion: i256(20),
                    included_orders: vec![
                        IncludedOrderData {
                            id: order_id(0x41),
                            landed_order_data: LandedOrderData {
                                order: order_id(0x41),
                                unique_coinbase_profit: i256(20),
                                total_coinbase_profit: i256(10), // make sure that this is not used
                                error: None,
                                overlapping_txs: vec![],
                            },
                        },
                        IncludedOrderData {
                            id: order_id(0x42),
                            landed_order_data: LandedOrderData {
                                order: order_id(0x41),
                                unique_coinbase_profit: i256(-10),
                                total_coinbase_profit: i256(-10),
                                error: None,
                                overlapping_txs: vec![],
                            },
                        },
                    ],
                },
                // identity with 2 value, hube backtest contribution but small onchain
                RedistributionIdentityData {
                    address: addr(5),
                    value_delta_after_exclusion: i256(4000),
                    included_orders: vec![
                        IncludedOrderData {
                            id: order_id(0x51),
                            landed_order_data: LandedOrderData {
                                order: order_id(0x51),
                                unique_coinbase_profit: i256(30),
                                total_coinbase_profit: i256(30),
                                error: None,
                                overlapping_txs: vec![],
                            },
                        },
                        IncludedOrderData {
                            id: order_id(0x52),
                            landed_order_data: LandedOrderData {
                                order: order_id(0x52),
                                unique_coinbase_profit: i256(50),
                                total_coinbase_profit: i256(50),
                                error: None,
                                overlapping_txs: vec![],
                            },
                        },
                    ],
                },
            ],
        };

        let output = RedistributionResult {
            landed_block_profit: u256(1000),
            total_value_redistributed: u256(110),
            value_by_identity: vec![
                RedistributionEntityResult {
                    address: addr(1),
                    value_received: u256(10),
                    order_contributions: vec![(order_id(0x11), u256(10))],
                },
                RedistributionEntityResult {
                    address: addr(2),
                    value_received: u256(0),
                    order_contributions: vec![(order_id(0x21), u256(0))],
                },
                RedistributionEntityResult {
                    address: addr(3),
                    value_received: u256(0),
                    order_contributions: vec![],
                },
                RedistributionEntityResult {
                    address: addr(4),
                    value_received: u256(20),
                    order_contributions: vec![
                        (order_id(0x41), u256(20)),
                        (order_id(0x42), u256(0)),
                    ],
                },
                RedistributionEntityResult {
                    address: addr(5),
                    value_received: u256(80),
                    order_contributions: vec![
                        (order_id(0x51), u256(30)),
                        (order_id(0x52), u256(50)),
                    ],
                },
            ],
        };

        assert_eq!(calculate_redistribution(input), output);
    }
}
