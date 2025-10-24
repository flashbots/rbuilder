use std::ops::Range;

use crate::utils::get_percent;
use ahash::HashMap;
use alloy_primitives::{B256, I256, U256};
use rbuilder_primitives::{Order, OrderId, ShareBundleBody, ShareBundleInner, TxRevertBehavior};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct OrderTxData {
    pub hash: B256,
    pub revert: TxRevertBehavior,
    pub kickback_percent: usize,
}

impl OrderTxData {
    fn new(hash: B256, revert: TxRevertBehavior, kickback_percent: usize) -> Self {
        OrderTxData {
            hash,
            revert,
            kickback_percent,
        }
    }
}

/// SimplifiedOrder represents unified form of the order
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SimplifiedOrder {
    pub id: OrderId,
    pub txs: Vec<OrderTxData>,
}

impl SimplifiedOrder {
    pub fn new(id: OrderId, txs: Vec<OrderTxData>) -> Self {
        SimplifiedOrder { id, txs }
    }

    pub fn new_from_order(order: &Order) -> Self {
        let id = order.id();
        match order {
            Order::Tx(tx) => SimplifiedOrder::new(
                id,
                vec![OrderTxData::new(
                    tx.tx_with_blobs.hash(),
                    TxRevertBehavior::AllowedIncluded,
                    0,
                )],
            ),
            Order::Bundle(bundle) => {
                let (refund_percent, refund_payer_hash) = if let Some(refund) = &bundle.refund {
                    (refund.percent as usize, Some(refund.tx_hash))
                } else {
                    (0, None)
                };
                let txs = order
                    .list_txs_revert()
                    .into_iter()
                    .map(|(tx, revert)| {
                        let tx_refund_percent = if Some(tx.hash()) == refund_payer_hash {
                            refund_percent
                        } else {
                            0
                        };
                        OrderTxData::new(tx.hash(), revert, tx_refund_percent)
                    })
                    .collect();
                SimplifiedOrder::new(id, txs)
            }
            Order::ShareBundle(bundle) => {
                SimplifiedOrder::new(id, order_txs_from_inner_share_bundle(bundle.inner_bundle()))
            }
        }
    }
}

pub fn order_txs_from_inner_share_bundle(inner: &ShareBundleInner) -> Vec<OrderTxData> {
    let total_refund_percent = inner.refund.iter().map(|r| r.percent).sum::<usize>();

    let mut accumulated_txs = Vec::new();

    let mut prev_element_paid_refund = false;
    let mut current_chunk_txs = Vec::new();

    let release_chunk = |current_chunk_txs: &mut Vec<(B256, TxRevertBehavior)>,
                         accumulated_txs: &mut Vec<OrderTxData>,
                         kickback_percent| {
        if !current_chunk_txs.is_empty() {
            for (hash, revert) in current_chunk_txs.drain(..) {
                accumulated_txs.push(OrderTxData::new(hash, revert, kickback_percent));
            }
        }
    };

    for (idx, body) in inner.body.iter().enumerate() {
        let current_element_pays_refund = !inner.refund.iter().any(|r| r.body_idx == idx);

        if prev_element_paid_refund != current_element_pays_refund {
            let chunk_refund_percent = if prev_element_paid_refund {
                total_refund_percent
            } else {
                0
            };
            release_chunk(
                &mut current_chunk_txs,
                &mut accumulated_txs,
                chunk_refund_percent,
            );
            prev_element_paid_refund = current_element_pays_refund;
        }

        match body {
            ShareBundleBody::Tx(tx) => {
                current_chunk_txs.push((tx.hash(), tx.revert_behavior));
            }
            ShareBundleBody::Bundle(inner_bundle) => {
                let chunk_refund_percent = if prev_element_paid_refund {
                    total_refund_percent
                } else {
                    0
                };
                release_chunk(
                    &mut current_chunk_txs,
                    &mut accumulated_txs,
                    chunk_refund_percent,
                );

                let mut inner_txs = order_txs_from_inner_share_bundle(inner_bundle);
                for tx in &mut inner_txs {
                    if current_element_pays_refund {
                        tx.kickback_percent =
                            multiply_inner_refunds(tx.kickback_percent, chunk_refund_percent);
                    }
                }
                accumulated_txs.extend(inner_txs);
            }
        }
    }

    let chunk_refund_percent = if prev_element_paid_refund {
        total_refund_percent
    } else {
        0
    };
    release_chunk(
        &mut current_chunk_txs,
        &mut accumulated_txs,
        chunk_refund_percent,
    );

    accumulated_txs
}

fn multiply_inner_refunds(a: usize, b: usize) -> usize {
    if a > 100 || b > 100 {
        return 0;
    }
    100 - (100 - a) * (100 - b) / 100
}

/// ExecutedBlockTx is data from the tx executed in the block
#[derive(Debug, Clone)]
pub struct ExecutedBlockTx {
    pub hash: B256,
    pub coinbase_profit: I256,
    pub success: bool,
}

impl ExecutedBlockTx {
    pub fn new(hash: B256, coinbase_profit: I256, success: bool) -> Self {
        ExecutedBlockTx {
            hash,
            coinbase_profit,
            success,
        }
    }
}

/// LandedOrderData is info about order that was restored from the block
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LandedOrderData {
    pub order: OrderId,
    pub total_coinbase_profit: I256,
    /// Unique profit is the profit that is unique to this order and its does not overlap with other landed orders
    /// For example, if we merged two backruns for one tx it only count backrun tx profit for each order
    pub unique_coinbase_profit: I256,
    pub error: Option<OrderIdentificationError>,
    pub overlapping_txs: Vec<(OrderId, B256)>,
    pub tx_hashes: Vec<B256>,
}

#[derive(Debug, Clone, thiserror::Error, Eq, PartialEq)]
pub enum OrderIdentificationError {
    #[error("Tx not found: {0}")]
    TxNotFound(B256),
    #[error("Tx reverted: {0}")]
    TxReverted(B256),
    #[error("Tx is in incorrect position: {0}")]
    TxIsIncorrectPosition(B256),
    #[error("No landed txs found")]
    NoOrderTxs,
}

impl LandedOrderData {
    pub fn new(
        order: OrderId,
        total_coinbase_profit: I256,
        unique_coinbase_profit: I256,
        error: Option<OrderIdentificationError>,
        overlapping_txs: Vec<(OrderId, B256)>,
        tx_hashes: Vec<B256>,
    ) -> Self {
        LandedOrderData {
            order,
            total_coinbase_profit,
            unique_coinbase_profit,
            error,
            overlapping_txs,
            tx_hashes,
        }
    }
}

#[derive(Debug)]
struct ExecutedBlockData {
    block_txs: Vec<ExecutedBlockTx>,
    txs_by_hash: HashMap<B256, usize>,
}

impl ExecutedBlockData {
    fn new_from_txs(txs: Vec<ExecutedBlockTx>) -> Self {
        let mut txs_by_hash = HashMap::default();
        for (idx, tx) in txs.iter().enumerate() {
            txs_by_hash.insert(tx.hash, idx);
        }
        ExecutedBlockData {
            block_txs: txs,
            txs_by_hash,
        }
    }

    fn find_tx(&self, hash: B256) -> Option<(usize, &ExecutedBlockTx)> {
        self.txs_by_hash
            .get(&hash)
            .map(|idx| (*idx, &self.block_txs[*idx]))
    }

    fn tx_coinbase_profit(&self, hash: B256) -> Option<I256> {
        self.find_tx(hash).map(|(_, tx)| tx.coinbase_profit)
    }
}

pub fn restore_landed_orders(
    block_txs: Vec<ExecutedBlockTx>,
    orders: Vec<SimplifiedOrder>,
) -> HashMap<OrderId, LandedOrderData> {
    let tx_to_index: HashMap<B256, usize> = block_txs
        .iter()
        .enumerate()
        .map(|(idx, tx)| (tx.hash, idx))
        .collect();

    let executed_block_data = ExecutedBlockData::new_from_txs(block_txs);

    let mut result = HashMap::default();

    let mut txs_to_orders: HashMap<B256, Vec<(OrderId, usize)>> = HashMap::default();

    for order in orders {
        match find_landed_order_data(&executed_block_data, &order) {
            Ok(data) => {
                for (tx, kickback) in data.landed_txs {
                    txs_to_orders
                        .entry(tx)
                        .or_default()
                        .push((order.id, kickback));
                }
            }
            Err(e) => {
                result.insert(
                    order.id,
                    LandedOrderData::new(
                        order.id,
                        I256::ZERO,
                        I256::ZERO,
                        Some(e),
                        Vec::new(),
                        Vec::new(),
                    ),
                );
            }
        }
    }

    for (tx, orders) in txs_to_orders {
        let profit = executed_block_data.tx_coinbase_profit(tx).unwrap();
        for (order, kickback) in &orders {
            let order = *order;
            let entry = result.entry(order).or_insert(LandedOrderData::new(
                order,
                I256::ZERO,
                I256::ZERO,
                None,
                Vec::new(),
                Vec::new(),
            ));
            entry.tx_hashes.push(tx);
            let profit = if *kickback == 0 {
                profit
            } else {
                let profit: U256 = profit.try_into().unwrap_or_default();
                let kickback = get_percent(profit, *kickback);
                profit
                    .checked_sub(kickback)
                    .unwrap_or_default()
                    .try_into()
                    .unwrap_or_default()
            };
            entry.total_coinbase_profit += profit;
            if orders.len() == 1 {
                entry.unique_coinbase_profit += profit;
            } else {
                entry.overlapping_txs = orders
                    .iter()
                    .filter_map(|(o, _)| if *o != order { Some((*o, tx)) } else { None })
                    .collect();
            }
        }
    }

    for landed_data in result.values_mut() {
        landed_data.overlapping_txs.sort_by_key(|(d, _)| *d);
        landed_data
            .tx_hashes
            .sort_by_key(|tx| tx_to_index.get(tx).unwrap());
    }

    result
}

#[derive(Debug)]
struct FoundOrderData {
    /// (tx_hash, kickback_percent)
    landed_txs: Vec<(B256, usize)>,
}

fn find_landed_order_data(
    block_data: &ExecutedBlockData,
    order: &SimplifiedOrder,
) -> Result<FoundOrderData, OrderIdentificationError> {
    // first we do a pass over chunk txs and try to locate all included txs
    #[derive(Debug)]
    struct FoundTxData {
        block_idx: usize,
        order_idx: usize,
        tx: OrderTxData,
        executed_block_tx: ExecutedBlockTx,
    }
    let mut found_txs = Vec::new();
    for (order_idx, tx) in order.txs.iter().enumerate() {
        let (block_idx, tx_data) = if let Some((idx, tx_data)) = block_data.find_tx(tx.hash) {
            (idx, tx_data)
        } else {
            // tx was not found in the block
            if tx.revert.can_revert() {
                continue;
            } else {
                // tx not found
                return Err(OrderIdentificationError::TxNotFound(tx.hash));
            }
        };
        found_txs.push(FoundTxData {
            block_idx,
            order_idx,
            tx: tx.clone(),
            executed_block_tx: tx_data.clone(),
        });
    }

    found_txs.sort_by_key(|txs| txs.order_idx);
    // check if non-optional txs are in order
    let mut last_block_idx = 0;
    let mut found_txs_block_locations: Vec<Option<usize>> = vec![None; order.txs.len()];
    for found_tx in &found_txs {
        if found_tx.tx.revert.can_revert() {
            continue;
        }
        if found_tx.block_idx < last_block_idx {
            return Err(OrderIdentificationError::TxIsIncorrectPosition(
                found_tx.tx.hash,
            ));
        }
        found_txs_block_locations[found_tx.order_idx] = Some(found_tx.block_idx);
        last_block_idx = found_tx.block_idx;
    }
    // now go over all optional txs and try to locate them
    let mut result = Vec::new();
    for found_tx in found_txs {
        if found_txs_block_locations[found_tx.order_idx].is_some() {
            result.push(found_tx);
            continue;
        }
        let allowed_block_range = find_allowed_range(
            block_data.block_txs.len(),
            found_tx.order_idx,
            &found_txs_block_locations,
        );
        if allowed_block_range.contains(&found_tx.block_idx) {
            found_txs_block_locations[found_tx.order_idx] = Some(found_tx.block_idx);
            result.push(found_tx);
        }
    }

    for found_tx in &result {
        if !found_tx.executed_block_tx.success && !found_tx.tx.revert.can_revert() {
            return Err(OrderIdentificationError::TxReverted(found_tx.tx.hash));
        }
    }

    let landed_txs = result
        .iter()
        .map(|d| (d.tx.hash, d.tx.kickback_percent))
        .collect();
    Ok(FoundOrderData { landed_txs })
}

fn find_allowed_range(
    block_len: usize,
    chunk_idx: usize,
    chunk_txs_block_idx: &[Option<usize>],
) -> Range<usize> {
    let upper_bound = chunk_txs_block_idx[chunk_idx..].iter().find_map(|d| *d);
    let lower_bound = chunk_txs_block_idx[..chunk_idx]
        .iter()
        .rfind(|d| d.is_some())
        .map(|d| d.unwrap() + 1);
    lower_bound.unwrap_or_default()..upper_bound.unwrap_or(block_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::test_utils::*;
    use rbuilder_primitives::{
        Bundle, BundleRefund, MempoolTx, Refund, ShareBundle, ShareBundleTx, LAST_BUNDLE_VERSION,
    };

    #[test]
    fn test_find_allowed_range() {
        let block_len = 100;
        let cases: Vec<(usize, Vec<Option<usize>>, Range<usize>)> = vec![
            (0, vec![None], 0..block_len),
            (0, vec![None, Some(12)], 0..12),
            (1, vec![Some(12), None], 13..block_len),
            (1, vec![Some(12), None, Some(14)], 13..14),
            (2, vec![Some(10), Some(12), None, Some(14)], 13..14),
        ];
        for (idx, (chunk_idx, chunk_txs_block_idx, expected)) in cases.into_iter().enumerate() {
            let got = find_allowed_range(block_len, chunk_idx, &chunk_txs_block_idx);
            assert_eq!(expected, got, "Test index: {idx}");
        }
    }

    fn assert_result(
        executed_txs: Vec<ExecutedBlockTx>,
        orders: Vec<SimplifiedOrder>,
        expected: Vec<LandedOrderData>,
    ) {
        let got = restore_landed_orders(executed_txs, orders);
        assert_eq!(expected.len(), got.len());
        for expected_result in expected {
            let got_result = got
                .get(&expected_result.order)
                .unwrap_or_else(|| panic!("Order not found: {expected_result:?}"));
            assert_eq!(expected_result, *got_result);
        }
    }

    #[test]
    fn test_simple_block_identification() {
        let executed_block = vec![
            // random mempool tx
            ExecutedBlockTx::new(hash(1), i256(11), true),
            // bundle 1 with 1 tx
            ExecutedBlockTx::new(hash(2), i256(12), true),
            // bundle 2 with 2/3 landed txs
            ExecutedBlockTx::new(hash(3), i256(13), false), // tx can revert
            ExecutedBlockTx::new(hash(33), i256(14), true),
            // random mempool tx
            ExecutedBlockTx::new(hash(4), i256(14), true),
            // bundle with simple kickback
            ExecutedBlockTx::new(hash(5), i256(15), true),
            ExecutedBlockTx::new(hash(6), i256(16), true), // backrun 1
            ExecutedBlockTx::new(hash(7), i256(-14), true), // kickback payout
            // last tx in the block
            ExecutedBlockTx::new(hash(8), i256(-20), true),
        ];

        let orders = vec![
            // bundle 1 with 1 tx
            SimplifiedOrder::new(
                order_id(0xb1),
                vec![OrderTxData::new(hash(2), TxRevertBehavior::NotAllowed, 0)],
            ),
            // bundle 2 with 2/3 landed txs
            SimplifiedOrder::new(
                order_id(0xb2),
                vec![
                    OrderTxData::new(hash(3), TxRevertBehavior::AllowedIncluded, 0),
                    OrderTxData::new(hash(33), TxRevertBehavior::NotAllowed, 0),
                    OrderTxData::new(hash(333), TxRevertBehavior::AllowedExcluded, 0), // this tx never landed
                ],
            ),
            // bundle with simple kickback
            SimplifiedOrder::new(
                order_id(0xb3),
                vec![
                    OrderTxData::new(hash(5), TxRevertBehavior::AllowedIncluded, 0),
                    OrderTxData::new(hash(6), TxRevertBehavior::NotAllowed, 90),
                ],
            ),
        ];

        let results = vec![
            // bundle 1 with 1 tx
            LandedOrderData::new(
                order_id(0xb1),
                i256(12),
                i256(12),
                None,
                vec![],
                vec![hash(2)],
            ),
            // bundle 2 with 2/3 landed txs
            LandedOrderData::new(
                order_id(0xb2),
                i256(13 + 14),
                i256(13 + 14),
                None,
                vec![],
                vec![hash(3), hash(33)],
            ),
            // bundle with simple kickback
            LandedOrderData::new(
                order_id(0xb3),
                i256(15 + 2),
                i256(15 + 2),
                None,
                vec![],
                vec![hash(5), hash(6)],
            ),
        ];
        assert_result(executed_block, orders, results);
    }

    #[test]
    fn test_merged_sandwich_identification() {
        // although this version of builder does not do that, this is possible
        // in this hypothetical scenario we have two sandwiches like
        // bundle_1: tx1_1, tx_mempool, tx1_2
        // bundle_2: tx2_1, tx_mempool, tx2_2
        // included txs: tx1_1, tx2_1, tx_mempool, tx1_2, tx2_2
        let executed_block = vec![
            ExecutedBlockTx::new(hash(0x11), i256(0x11), true),
            ExecutedBlockTx::new(hash(0x21), i256(0x21), true),
            ExecutedBlockTx::new(hash(0xaa), i256(0xaa), true),
            ExecutedBlockTx::new(hash(0x12), i256(0x12), true),
            ExecutedBlockTx::new(hash(0x22), i256(0x22), true),
        ];

        let orders = vec![
            SimplifiedOrder::new(
                order_id(0xb1),
                vec![
                    OrderTxData::new(hash(0x11), TxRevertBehavior::NotAllowed, 0),
                    OrderTxData::new(hash(0xaa), TxRevertBehavior::NotAllowed, 0),
                    OrderTxData::new(hash(0x12), TxRevertBehavior::NotAllowed, 0),
                ],
            ),
            SimplifiedOrder::new(
                order_id(0xb2),
                vec![
                    OrderTxData::new(hash(0x21), TxRevertBehavior::NotAllowed, 0),
                    OrderTxData::new(hash(0xaa), TxRevertBehavior::NotAllowed, 0),
                    OrderTxData::new(hash(0x22), TxRevertBehavior::NotAllowed, 0),
                ],
            ),
        ];

        let results = vec![
            LandedOrderData::new(
                order_id(0xb1),
                i256(0x11 + 0xaa + 0x12),
                i256(0x11 + 0x12),
                None,
                vec![(order_id(0xb2), hash(0xaa))],
                vec![hash(0x11), hash(0xaa), hash(0x12)],
            ),
            LandedOrderData::new(
                order_id(0xb2),
                i256(0x21 + 0xaa + 0x22),
                i256(0x21 + 0x22),
                None,
                vec![(order_id(0xb1), hash(0xaa))],
                vec![hash(0x21), hash(0xaa), hash(0x22)],
            ),
        ];
        assert_result(executed_block, orders, results);
    }

    #[test]
    fn test_merged_backruns_identification() {
        // bundle_1: tx0
        // bundle_2: tx0, tx2 (backrun = 90)
        // bundle_3: tx0, tx3 (backrun = 80)
        // included txs: tx0, tx3, tx2
        let executed_block = vec![
            ExecutedBlockTx::new(hash(0x00), i256(12), true),
            ExecutedBlockTx::new(hash(0x03), i256(2000), true),
            ExecutedBlockTx::new(hash(0x02), i256(1000), true),
        ];

        let orders = vec![
            SimplifiedOrder::new(
                order_id(0xb1),
                vec![OrderTxData::new(
                    hash(0x00),
                    TxRevertBehavior::NotAllowed,
                    0,
                )],
            ),
            SimplifiedOrder::new(
                order_id(0xb2),
                vec![
                    OrderTxData::new(hash(0x00), TxRevertBehavior::NotAllowed, 0),
                    OrderTxData::new(hash(0x02), TxRevertBehavior::NotAllowed, 90),
                ],
            ),
            SimplifiedOrder::new(
                order_id(0xb3),
                vec![
                    // this is how we merge backruns now
                    OrderTxData::new(hash(0x00), TxRevertBehavior::AllowedExcluded, 0),
                    OrderTxData::new(hash(0x03), TxRevertBehavior::NotAllowed, 80),
                ],
            ),
        ];

        let results = vec![
            LandedOrderData::new(
                order_id(0xb1),
                i256(12),
                i256(0),
                None,
                vec![(order_id(0xb2), hash(0x00)), (order_id(0xb3), hash(0x00))],
                vec![hash(0x00)],
            ),
            LandedOrderData::new(
                order_id(0xb2),
                i256(12 + 100),
                i256(100),
                None,
                vec![(order_id(0xb1), hash(0x00)), (order_id(0xb3), hash(0x00))],
                vec![hash(0x00), hash(0x02)],
            ),
            LandedOrderData::new(
                order_id(0xb3),
                i256(12 + 400),
                i256(400),
                None,
                vec![(order_id(0xb1), hash(0x00)), (order_id(0xb2), hash(0x00))],
                vec![hash(0x00), hash(0x03)],
            ),
        ];
        assert_result(executed_block, orders, results);
    }

    #[test]
    fn test_bundle_identification_errors() {
        let executed_block = vec![
            ExecutedBlockTx::new(hash(0x01), i256(11), false),
            ExecutedBlockTx::new(hash(0x02), i256(12), true),
            ExecutedBlockTx::new(hash(0x03), i256(12), true),
        ];

        let orders = vec![
            SimplifiedOrder::new(
                order_id(0xb1),
                vec![OrderTxData::new(
                    hash(0x01),
                    TxRevertBehavior::NotAllowed,
                    0,
                )],
            ),
            SimplifiedOrder::new(
                order_id(0xb2),
                vec![
                    OrderTxData::new(hash(0x02), TxRevertBehavior::NotAllowed, 0),
                    OrderTxData::new(hash(0xAA), TxRevertBehavior::NotAllowed, 90),
                ],
            ),
            SimplifiedOrder::new(
                order_id(0xb3),
                vec![
                    OrderTxData::new(hash(0x03), TxRevertBehavior::NotAllowed, 0),
                    OrderTxData::new(hash(0x02), TxRevertBehavior::NotAllowed, 0),
                ],
            ),
            SimplifiedOrder::new(
                order_id(0xb4),
                vec![
                    // this is how we merge backruns now
                    OrderTxData::new(hash(0x03), TxRevertBehavior::NotAllowed, 0),
                    OrderTxData::new(hash(0x02), TxRevertBehavior::NotAllowed, 80),
                ],
            ),
            SimplifiedOrder::new(
                order_id(0xb5),
                vec![OrderTxData::new(
                    hash(0x01),
                    TxRevertBehavior::NotAllowed,
                    0,
                )],
            ),
        ];

        let results = vec![
            LandedOrderData::new(
                order_id(0xb1),
                i256(0),
                i256(0),
                Some(OrderIdentificationError::TxReverted(hash(0x01))),
                vec![],
                vec![],
            ),
            LandedOrderData::new(
                order_id(0xb2),
                i256(0),
                i256(0),
                Some(OrderIdentificationError::TxNotFound(hash(0xAA))),
                vec![],
                vec![],
            ),
            LandedOrderData::new(
                order_id(0xb3),
                i256(0),
                i256(0),
                Some(OrderIdentificationError::TxIsIncorrectPosition(hash(0x02))),
                vec![],
                vec![],
            ),
            LandedOrderData::new(
                order_id(0xb4),
                i256(0),
                i256(0),
                Some(OrderIdentificationError::TxIsIncorrectPosition(hash(0x02))),
                vec![],
                vec![],
            ),
            LandedOrderData::new(
                order_id(0xb5),
                i256(0),
                i256(0),
                Some(OrderIdentificationError::TxReverted(hash(0x01))),
                vec![],
                vec![],
            ),
        ];
        assert_result(executed_block, orders, results);
    }

    #[test]
    fn test_out_of_order_droppable_txs() {
        // bundle_1: tx1_1 (optional), tx1_2 (optional), tx1_3
        // included txs: tx1_2, tx1_1, tx1_3
        let executed_block = vec![
            ExecutedBlockTx::new(hash(0x12), i256(0x12), true),
            ExecutedBlockTx::new(hash(0x11), i256(0x11), true),
            ExecutedBlockTx::new(hash(0x13), i256(0x13), true),
        ];

        let orders = vec![SimplifiedOrder::new(
            order_id(0xb1),
            vec![
                OrderTxData::new(hash(0x11), TxRevertBehavior::AllowedExcluded, 0),
                OrderTxData::new(hash(0x12), TxRevertBehavior::AllowedExcluded, 0),
                OrderTxData::new(hash(0x13), TxRevertBehavior::NotAllowed, 0),
            ],
        )];

        let results = vec![LandedOrderData::new(
            order_id(0xb1),
            i256(0x11 + 0x13),
            i256(0x11 + 0x13),
            None,
            vec![],
            vec![hash(0x11), hash(0x13)],
        )];
        assert_result(executed_block, orders, results);
    }

    #[test]
    fn test_backruns_recorvery_with_out_of_order_txs() {
        // bundle_i column indicate index of tx inside bundle and allow status
        //
        // bundle_0                         bundle_1_idx              hash
        // ----------------------------------------------------------
        // bundle_0:1:allow_included        bundle_1:0:not_allowed    0x1
        // bundle_0:0:allow_included        bundle_1:1:not_allowed    0x2
        //                                  bundle_1:2:not_allowed    0x3
        // bundle_0:2:allow_included                                  0x4
        // bundle_0:3:not_allowed:backrun                             0x5
        let executed_block = vec![
            ExecutedBlockTx::new(hash(0x1), i256(0x1), true),
            ExecutedBlockTx::new(hash(0x2), i256(0x2), true),
            ExecutedBlockTx::new(hash(0x3), i256(0x3), true),
            ExecutedBlockTx::new(hash(0x4), i256(0x4), true),
            ExecutedBlockTx::new(hash(0x5), i256(10), true),
        ];

        let orders = vec![
            SimplifiedOrder::new(
                order_id(0xb0),
                vec![
                    OrderTxData::new(hash(0x2), TxRevertBehavior::AllowedIncluded, 0),
                    OrderTxData::new(hash(0x1), TxRevertBehavior::AllowedIncluded, 0),
                    OrderTxData::new(hash(0x4), TxRevertBehavior::AllowedIncluded, 0),
                    OrderTxData::new(hash(0x5), TxRevertBehavior::NotAllowed, 50),
                ],
            ),
            SimplifiedOrder::new(
                order_id(0xb1),
                vec![
                    OrderTxData::new(hash(0x1), TxRevertBehavior::NotAllowed, 0),
                    OrderTxData::new(hash(0x2), TxRevertBehavior::NotAllowed, 0),
                    OrderTxData::new(hash(0x3), TxRevertBehavior::NotAllowed, 0),
                ],
            ),
        ];

        let results = vec![
            LandedOrderData::new(
                order_id(0xb0),
                i256(0x2 + 0x4 + 10 / 2),
                i256(0x4 + 10 / 2),
                None,
                vec![(order_id(0xb1), hash(0x2))],
                vec![hash(0x2), hash(0x4), hash(0x5)],
            ),
            LandedOrderData::new(
                order_id(0xb1),
                i256(0x1 + 0x2 + 0x3),
                i256(0x1 + 0x3),
                None,
                vec![(order_id(0xb0), hash(0x2))],
                vec![hash(0x1), hash(0x2), hash(0x3)],
            ),
        ];
        assert_result(executed_block, orders, results);
    }

    #[test]
    fn test_backruns_recorvery_with_out_of_order_txs_case_2() {
        // bundle_i column indicate index of tx inside bundle and allow status
        //
        // bundle_0                         bundle_1_idx                 hash
        // -----------------------------------------------------------------
        // bundle_0:1:not_allowed:backrun                                0x1
        // bundle_0:0:allow_excluded        bundle_1:0:allow_excluded    0x2
        let executed_block = vec![
            ExecutedBlockTx::new(hash(0x1), i256(10), true),
            ExecutedBlockTx::new(hash(0x2), i256(0x2), true),
        ];

        let orders = vec![
            SimplifiedOrder::new(
                order_id(0xb0),
                vec![
                    OrderTxData::new(hash(0x2), TxRevertBehavior::AllowedExcluded, 0),
                    OrderTxData::new(hash(0x1), TxRevertBehavior::NotAllowed, 50),
                ],
            ),
            SimplifiedOrder::new(
                order_id(0xb1),
                vec![OrderTxData::new(
                    hash(0x2),
                    TxRevertBehavior::AllowedExcluded,
                    0,
                )],
            ),
        ];

        let results = vec![
            LandedOrderData::new(
                order_id(0xb0),
                i256(10 / 2),
                i256(10 / 2),
                None,
                vec![],
                vec![hash(0x1)],
            ),
            LandedOrderData::new(
                order_id(0xb1),
                i256(0x2),
                i256(0x2),
                None,
                vec![],
                vec![hash(0x2)],
            ),
        ];
        assert_result(executed_block, orders, results);
    }

    #[test]
    fn test_allow_included_tx_dropped() {
        // this case can happen if tx with AllowIncluded was dropped because nonce is invalid
        // we must allow skipping these kind of txs
        let executed_block = vec![ExecutedBlockTx::new(hash(0x1), i256(0x1), true)];

        let orders = vec![SimplifiedOrder::new(
            order_id(0xb1),
            vec![
                OrderTxData::new(hash(0xc), TxRevertBehavior::AllowedIncluded, 0),
                OrderTxData::new(hash(0x1), TxRevertBehavior::NotAllowed, 0),
            ],
        )];

        let results = vec![LandedOrderData::new(
            order_id(0xb1),
            i256(0x1),
            i256(0x1),
            None,
            vec![],
            vec![hash(0x1)],
        )];
        assert_result(executed_block, orders, results);
    }

    #[test]
    fn test_simplified_order_conversion_mempool_tx() {
        let order = Order::Tx(MempoolTx {
            tx_with_blobs: tx(0x01),
        });
        let expected = SimplifiedOrder::new(
            OrderId::Tx(hash(0x01)),
            vec![OrderTxData::new(
                hash(0x01),
                TxRevertBehavior::AllowedIncluded,
                0,
            )],
        );

        let got = SimplifiedOrder::new_from_order(&order);
        assert_eq!(expected, got);
    }

    #[test]
    fn test_simplified_order_conversion_bundle() {
        let bundle = Order::Bundle(Bundle {
            block: Some(0),
            min_timestamp: None,
            max_timestamp: None,
            txs: vec![tx(0x01), tx(0x02)],
            reverting_tx_hashes: vec![hash(0x02)],
            hash: Default::default(),
            uuid: uuid::uuid!("00000000-0000-0000-0000-ffff00000002"),
            replacement_data: None,
            signer: None,
            metadata: Default::default(),
            dropping_tx_hashes: Default::default(),
            refund: Default::default(),
            refund_identity: None,
            version: LAST_BUNDLE_VERSION,
            external_hash: None,
        });
        let expected = SimplifiedOrder::new(
            OrderId::Bundle(uuid::uuid!("00000000-0000-0000-0000-ffff00000002")),
            vec![
                OrderTxData::new(hash(0x01), TxRevertBehavior::NotAllowed, 0),
                OrderTxData::new(hash(0x02), TxRevertBehavior::AllowedIncluded, 0),
            ],
        );

        let got = SimplifiedOrder::new_from_order(&bundle);
        assert_eq!(expected, got);
    }

    #[test]
    fn test_simplified_order_conversion_bundle_with_refund() {
        let bundle = Order::Bundle(Bundle {
            block: Some(0),
            min_timestamp: None,
            max_timestamp: None,
            txs: vec![tx(0x01), tx(0x02)],
            reverting_tx_hashes: vec![hash(0x02)],
            hash: Default::default(),
            uuid: uuid::uuid!("00000000-0000-0000-0000-ffff00000002"),
            replacement_data: None,
            signer: None,
            metadata: Default::default(),
            dropping_tx_hashes: Default::default(),
            refund: Some(BundleRefund {
                percent: 10,
                recipient: Default::default(),
                tx_hash: hash(0x02),
                delayed: false,
            }),
            refund_identity: None,
            version: LAST_BUNDLE_VERSION,
            external_hash: None,
        });
        let expected = SimplifiedOrder::new(
            OrderId::Bundle(uuid::uuid!("00000000-0000-0000-0000-ffff00000002")),
            vec![
                OrderTxData::new(hash(0x01), TxRevertBehavior::NotAllowed, 0),
                OrderTxData::new(hash(0x02), TxRevertBehavior::AllowedIncluded, 10),
            ],
        );

        let got = SimplifiedOrder::new_from_order(&bundle);
        assert_eq!(expected, got);
    }

    #[test]
    fn test_simplified_order_conversion_share_bundle() {
        let bundle = Order::ShareBundle(ShareBundle::new_with_fake_hash(
            hash(0xb1),
            0,
            0,
            ShareBundleInner {
                body: vec![
                    ShareBundleBody::Tx(ShareBundleTx {
                        tx: tx(0x01),
                        revert_behavior: TxRevertBehavior::NotAllowed,
                    }),
                    ShareBundleBody::Tx(ShareBundleTx {
                        tx: tx(0x02),
                        revert_behavior: TxRevertBehavior::AllowedExcluded,
                    }),
                    ShareBundleBody::Tx(ShareBundleTx {
                        tx: tx(0x03),
                        revert_behavior: TxRevertBehavior::AllowedIncluded,
                    }),
                    ShareBundleBody::Bundle(ShareBundleInner {
                        body: vec![
                            ShareBundleBody::Bundle(ShareBundleInner {
                                body: vec![ShareBundleBody::Tx(ShareBundleTx {
                                    tx: tx(0x11),
                                    revert_behavior: TxRevertBehavior::NotAllowed,
                                })],
                                refund: vec![],
                                refund_config: vec![],
                                can_skip: false,
                                original_order_id: None,
                            }),
                            ShareBundleBody::Tx(ShareBundleTx {
                                tx: tx(0x12),
                                revert_behavior: TxRevertBehavior::NotAllowed,
                            }),
                        ],
                        refund: vec![Refund {
                            body_idx: 0,
                            percent: 20,
                        }],
                        refund_config: vec![],
                        can_skip: true,
                        original_order_id: None,
                    }),
                    ShareBundleBody::Tx(ShareBundleTx {
                        tx: tx(0x04),
                        revert_behavior: TxRevertBehavior::AllowedIncluded,
                    }),
                ],
                refund: vec![
                    Refund {
                        body_idx: 0,
                        percent: 10,
                    },
                    Refund {
                        body_idx: 1,
                        percent: 20,
                    },
                    Refund {
                        body_idx: 4,
                        percent: 30,
                    },
                ],
                refund_config: vec![],
                can_skip: false,
                original_order_id: None,
            },
            None,
            None,
            vec![],
            Default::default(),
        ));
        let expected = SimplifiedOrder::new(
            OrderId::ShareBundle(hash(0xb1)),
            vec![
                OrderTxData::new(hash(0x01), TxRevertBehavior::NotAllowed, 0),
                OrderTxData::new(hash(0x02), TxRevertBehavior::AllowedExcluded, 0),
                OrderTxData::new(hash(0x03), TxRevertBehavior::AllowedIncluded, 60),
                OrderTxData::new(hash(0x11), TxRevertBehavior::NotAllowed, 60),
                OrderTxData::new(hash(0x12), TxRevertBehavior::NotAllowed, 68),
                OrderTxData::new(hash(0x04), TxRevertBehavior::AllowedIncluded, 0),
            ],
        );

        let got = SimplifiedOrder::new_from_order(&bundle);
        assert_eq!(got, expected);
    }
}
