// disable clippy::needless_range_loop for this file
#![allow(clippy::needless_range_loop)]

use alloy_primitives::{Address, I256, U256};
use rbuilder_primitives::OrderId;
use std::cmp::{max, min};

#[derive(Debug, Clone)]
pub struct IncludedOrderData {
    pub id: OrderId,
    /// b
    pub realized_value: U256,
}

#[derive(Debug, Clone)]
pub struct RedistributionIdentityData {
    pub address: Address,
    /// V(T) - V(T \ Order)
    pub block_value_delta: U256,
    pub included_orders: Vec<IncludedOrderData>,
}

#[derive(Debug, Clone)]
pub struct RedistributionCalculator {
    /// v(B(T)) - c
    pub landed_block_profit: U256,
    pub identity_data: Vec<RedistributionIdentityData>,
    /// First element of the tuple is some subset of identities,
    /// second is block value delta after excluding all identities from the subset
    pub joint_block_value_delta: Vec<((Address, Address), U256)>,
}

impl RedistributionCalculator {
    pub fn calculate_redistribution(self) -> RedistributionResult {
        calculate_redistribution(self)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RedistributionEntityResult {
    pub address: Address,
    pub redistribution_value: U256,
    /// Contribution to the value_received by each individual order
    pub order_redistribution_value: Vec<(OrderId, U256)>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RedistributionResult {
    /// Total value that is redistributed to different identities <= landed_block_profit
    pub total_value_redistributed: U256,
    pub value_by_identity: Vec<RedistributionEntityResult>,
}

fn vector_subset_data(data: &RedistributionCalculator) -> (Vec<(usize, usize)>, Vec<U256>) {
    let mut idx = Vec::new();
    let mut values = Vec::new();
    for ((addr1, addr2), value) in &data.joint_block_value_delta {
        let idx1 = if let Some(idx) = data.identity_data.iter().position(|i| i.address == *addr1) {
            idx
        } else {
            continue;
        };
        let idx2 = if let Some(idx) = data.identity_data.iter().position(|i| i.address == *addr2) {
            idx
        } else {
            continue;
        };
        idx.push((idx1, idx2));
        values.push(*value);
    }
    (idx, values)
}

pub fn calculate_redistribution(data: RedistributionCalculator) -> RedistributionResult {
    let identity = &data.identity_data;
    let n = identity.len();
    // b_i(T)
    let mut realized_value = vec![U256::ZERO; n];
    for i in 0..n {
        realized_value[i] = identity[i]
            .included_orders
            .iter()
            .map(|o| o.realized_value)
            .sum();
    }
    // mu_i(T)
    let mut marginal_contribution = vec![U256::ZERO; n];
    for i in 0..n {
        marginal_contribution[i] = min(realized_value[i], identity[i].block_value_delta);
    }

    let total_marginal_contribution: U256 = marginal_contribution.iter().sum();
    // min(v(B) - c, sum i mu_i)
    let value_to_split = min(data.landed_block_profit, total_marginal_contribution);

    // phi_i
    let flat_tax_redistibution_value = split_value(value_to_split, &marginal_contribution);

    // Identity rule
    // this is very crude approximation of identity rules and should be replaced with proper solver

    let (subsets, subset_block_value_delta) = vector_subset_data(&data);

    // mu_I
    let mut subset_joint_marginal_contribution = vec![U256::ZERO; subsets.len()];
    for j in 0..subsets.len() {
        let subset_value_paid = realized_value[subsets[j].0] + realized_value[subsets[j].1];
        subset_joint_marginal_contribution[j] = min(subset_value_paid, subset_block_value_delta[j]);
    }

    // r_i
    let mut identity_payments = flat_tax_redistibution_value;

    for j in 0..subsets.len() {
        // sum r_i
        let subset_payment = identity_payments[subsets[j].0] + identity_payments[subsets[j].1];
        let delta = if subset_payment > subset_joint_marginal_contribution[j] {
            subset_payment - subset_joint_marginal_contribution[j]
        } else {
            // we don't need to adjust that pair
            continue;
        };
        let (a, b) = (
            identity_payments[subsets[j].0],
            identity_payments[subsets[j].1],
        );
        let (a, b) = (I256::try_from(a).unwrap(), I256::try_from(b).unwrap());
        let (new_a, new_b) = adjust_contributions(a, b, I256::try_from(delta).unwrap());
        let (new_a, new_b) = (
            U256::try_from(new_a).unwrap(),
            U256::try_from(new_b).unwrap(),
        );
        identity_payments[subsets[j].0] = new_a;
        identity_payments[subsets[j].1] = new_b;
    }

    // assert that all pairwise constraints are now done
    for j in 0..subsets.len() {
        let subset_payment = identity_payments[subsets[j].0] + identity_payments[subsets[j].1];
        assert!(subset_payment <= subset_joint_marginal_contribution[j]);
    }

    let mut total_value_redistributed = U256::ZERO;
    let mut redistribution_entity_result = Vec::new();
    for i in 0..n {
        let mut order_id_vector = Vec::new();
        let mut order_contrib_vector = Vec::new();
        for landed_order in &data.identity_data[i].included_orders {
            order_id_vector.push(landed_order.id);
            order_contrib_vector.push(landed_order.realized_value);
        }
        let redistribution_value = identity_payments[i];
        let order_redistribution_values = split_value(redistribution_value, &order_contrib_vector);

        redistribution_entity_result.push(RedistributionEntityResult {
            address: data.identity_data[i].address,
            redistribution_value,
            order_redistribution_value: order_id_vector
                .into_iter()
                .zip(order_redistribution_values)
                .collect(),
        });
        total_value_redistributed += redistribution_value;
    }

    RedistributionResult {
        total_value_redistributed,
        value_by_identity: redistribution_entity_result,
    }
}

// this function solves the following problem
// given a, b
// subtract delta from a + b such that (a-a1)^2 + (b-b2)^2 is minimized
// a' = a - a1, b' = b - b2
// a1 + b2 = delta
// it returns a', b' i.e. new values for a and b
fn adjust_contributions(a: I256, b: I256, delta: I256) -> (I256, I256) {
    // we assume a <= b
    let (a, b, switch) = if a <= b { (a, b, false) } else { (b, a, true) };

    // 1. in case we need to subtract more than a + b we just zero contributions
    if delta >= a + b {
        return (I256::ZERO, I256::ZERO);
    }

    // we are minimizing (a-a1)^2 + (b - (delta - a1))^2 by a1
    // this is parabola going up, minimum is either the peak of the parabola
    // or on the left / right edge of the allowed interval depending on where the minimum is
    // some obvious constraints
    // a' >= 0 && a' <= a
    //   ->
    //   a1 >= 0
    //   a1 <= a
    //
    // b' >= 0 && b' <= b
    //   ->
    //   a1 <= delta
    //   a1 >= delta - b

    let parabola_min = (delta - b + a) / (I256::try_from(2).unwrap());

    let left_bound = max(delta - b, I256::ZERO);
    let right_bound = min(a, delta);

    let a_min = if parabola_min <= left_bound {
        left_bound
    } else if parabola_min > left_bound && parabola_min <= right_bound {
        parabola_min
    } else {
        right_bound
    };

    let a_new = a - a_min;
    let b_new = b - (delta - a_min);

    assert!(!a_new.is_negative());
    assert!(!b_new.is_negative());
    assert!(a_new + b_new <= a + b - delta);

    if switch {
        (b_new, a_new)
    } else {
        (a_new, b_new)
    }
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
    fn test_adjust_contributions() {
        // a - contribution 1 before
        // b - contribution 2 before
        // delta - how much to subtract from a + b
        // a' - contribution 1 after
        // b' - contribution 2 after
        // ((a, b), delta, ('a, b'))
        let test_data = vec![
            ((75, 100), 5, (75, 95)),
            ((75, 100), 20, (75, 80)),
            ((75, 100), 25, (75, 75)),
            ((75, 100), 30, (73, 72)),
            ((75, 100), 75, (50, 50)),
            ((75, 100), 90, (43, 42)),
            ((75, 100), 100, (38, 37)),
            ((75, 100), 170, (3, 2)),
            ((75, 100), 200, (0, 0)),
        ];

        for ((a, b), delta, expected) in test_data {
            let a = i256(a);
            let b = i256(b);
            let delta = i256(delta);
            let expected = (i256(expected.0), i256(expected.1));
            assert_eq!(adjust_contributions(a, b, delta), expected);

            let expected_switched = (expected.1, expected.0);
            assert_eq!(adjust_contributions(b, a, delta), expected_switched);
        }
    }

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
    fn test_calculate_redistribution_simple() {
        let input = RedistributionCalculator {
            landed_block_profit: u256(10000),
            identity_data: vec![
                RedistributionIdentityData {
                    address: addr(1),
                    block_value_delta: u256(100),
                    included_orders: vec![
                        IncludedOrderData {
                            id: order_id(1),
                            realized_value: u256(10),
                        },
                        IncludedOrderData {
                            id: order_id(2),
                            realized_value: u256(20),
                        },
                    ],
                },
                RedistributionIdentityData {
                    address: addr(2),
                    block_value_delta: u256(200),
                    included_orders: vec![IncludedOrderData {
                        id: order_id(3),
                        realized_value: u256(50),
                    }],
                },
            ],
            joint_block_value_delta: vec![],
        };

        let output = RedistributionResult {
            total_value_redistributed: u256(10 + 20 + 50),
            value_by_identity: vec![
                RedistributionEntityResult {
                    address: addr(1),
                    redistribution_value: u256(30),
                    order_redistribution_value: vec![
                        (order_id(1), u256(10)),
                        (order_id(2), u256(20)),
                    ],
                },
                RedistributionEntityResult {
                    address: addr(2),
                    redistribution_value: u256(50),
                    order_redistribution_value: vec![(order_id(3), u256(50))],
                },
            ],
        };

        assert_eq!(calculate_redistribution(input), output);
    }
    #[test]
    fn test_calculate_redistribution_less_profit_than_contribution() {
        let input = RedistributionCalculator {
            landed_block_profit: u256(1000),
            identity_data: vec![
                RedistributionIdentityData {
                    address: addr(1),
                    block_value_delta: u256(1000),
                    included_orders: vec![IncludedOrderData {
                        id: order_id(1),
                        realized_value: u256(1000),
                    }],
                },
                RedistributionIdentityData {
                    address: addr(2),
                    block_value_delta: u256(2000),
                    included_orders: vec![IncludedOrderData {
                        id: order_id(3),
                        realized_value: u256(2000),
                    }],
                },
            ],
            joint_block_value_delta: vec![],
        };

        let output = RedistributionResult {
            total_value_redistributed: u256(999),
            value_by_identity: vec![
                RedistributionEntityResult {
                    address: addr(1),
                    redistribution_value: u256(333),
                    order_redistribution_value: vec![(order_id(1), u256(333))],
                },
                RedistributionEntityResult {
                    address: addr(2),
                    redistribution_value: u256(666),
                    order_redistribution_value: vec![(order_id(3), u256(666))],
                },
            ],
        };
        assert_eq!(calculate_redistribution(input), output);
    }

    #[test]
    fn test_calculate_redistribution_double_identity_pairwise_constraint() {
        // we have 2 identities that are actually one split into 2 and one independent that pays 100
        //
        // we have bundle1 that pays 600 + bundles2 phat pays 400 and bundle2 depends on bundle1
        // there is another bundles that pays 500 for the same opportunity
        //
        // block value delta:
        // 1 -> 600 + 400 - 500 = 500
        // 2 -> 400
        // 3 -> 100
        //
        // joint block value delta:
        // (1,2) -> 600 + 400 - 500
        let input_no_joint_contribution_data = RedistributionCalculator {
            landed_block_profit: u256(1000),
            identity_data: vec![
                RedistributionIdentityData {
                    address: addr(1),
                    block_value_delta: u256(500),
                    included_orders: vec![IncludedOrderData {
                        id: order_id(1),
                        realized_value: u256(600),
                    }],
                },
                RedistributionIdentityData {
                    address: addr(2),
                    block_value_delta: u256(400),
                    included_orders: vec![IncludedOrderData {
                        id: order_id(2),
                        realized_value: u256(400),
                    }],
                },
                RedistributionIdentityData {
                    address: addr(3),
                    block_value_delta: u256(100),
                    included_orders: vec![IncludedOrderData {
                        id: order_id(3),
                        realized_value: u256(100),
                    }],
                },
            ],
            joint_block_value_delta: vec![],
        };

        // here 1 + 2 got 900 although really their marginal contribution is 500
        // thats why 3 only got 100
        let output_without_joint_constraint = RedistributionResult {
            total_value_redistributed: u256(500 + 400 + 100),
            value_by_identity: vec![
                RedistributionEntityResult {
                    address: addr(1),
                    redistribution_value: u256(500),
                    order_redistribution_value: vec![(order_id(1), u256(500))],
                },
                RedistributionEntityResult {
                    address: addr(2),
                    redistribution_value: u256(400),
                    order_redistribution_value: vec![(order_id(2), u256(400))],
                },
                RedistributionEntityResult {
                    address: addr(3),
                    redistribution_value: u256(100),
                    order_redistribution_value: vec![(order_id(3), u256(100))],
                },
            ],
        };

        assert_eq!(
            calculate_redistribution(input_no_joint_contribution_data.clone()),
            output_without_joint_constraint
        );

        let mut input_with_joint_contribution = input_no_joint_contribution_data;
        input_with_joint_contribution.joint_block_value_delta = vec![
            // 1 and 2 should now get at most 500
            ((addr(1), addr(2)), u256(500)),
        ];

        let output_with_joint_constraint = RedistributionResult {
            total_value_redistributed: u256(250 + 250 + 100),
            value_by_identity: vec![
                RedistributionEntityResult {
                    address: addr(1),
                    redistribution_value: u256(250),
                    order_redistribution_value: vec![(order_id(1), u256(250))],
                },
                RedistributionEntityResult {
                    address: addr(2),
                    redistribution_value: u256(250),
                    order_redistribution_value: vec![(order_id(2), u256(250))],
                },
                RedistributionEntityResult {
                    address: addr(3),
                    redistribution_value: u256(100),
                    order_redistribution_value: vec![(order_id(3), u256(100))],
                },
            ],
        };

        assert_eq!(
            calculate_redistribution(input_with_joint_contribution),
            output_with_joint_constraint
        );
    }
}
