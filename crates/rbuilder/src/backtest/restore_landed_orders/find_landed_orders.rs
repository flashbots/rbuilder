use crate::{
    primitives::{Order, OrderId, ShareBundleBody, ShareBundleInner, TxRevertBehavior},
    utils::get_percent,
};
use ahash::HashMap;
use alloy_primitives::{B256, I256, U256};

/// SimplifiedOrder represents unified form of the order
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SimplifiedOrder {
    pub id: OrderId,
    pub chunks: Vec<OrderChunk>,
}

impl SimplifiedOrder {
    pub fn new(id: OrderId, chunks: Vec<OrderChunk>) -> Self {
        SimplifiedOrder { id, chunks }
    }

    pub fn new_from_order(order: &Order) -> Self {
        let id = order.id();
        match order {
            Order::Tx(tx) => SimplifiedOrder::new(
                id,
                vec![OrderChunk::new(
                    vec![(tx.tx_with_blobs.hash(), TxRevertBehavior::AllowedIncluded)],
                    false,
                    0,
                )],
            ),
            Order::Bundle(_) => {
                let txs = order
                    .list_txs()
                    .into_iter()
                    .map(|(tx, optional)| {
                        let revert = if optional {
                            TxRevertBehavior::AllowedExcluded
                        } else {
                            TxRevertBehavior::NotAllowed
                        };
                        (tx.hash(), revert)
                    })
                    .collect();
                SimplifiedOrder::new(id, vec![OrderChunk::new(txs, false, 0)])
            }
            Order::ShareBundle(bundle) => SimplifiedOrder::new(
                id,
                OrderChunk::chunks_from_inner_share_bundle(&bundle.inner_bundle),
            ),
        }
    }
}

/// OrderChunk is atomic set of txs
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct OrderChunk {
    pub txs: Vec<(B256, TxRevertBehavior)>,
    /// If true this chunk can be dropped from the Order
    pub optional: bool,
    pub kickback_percent: usize,
}

impl OrderChunk {
    pub fn new(
        txs: Vec<(B256, TxRevertBehavior)>,
        optional: bool,
        kickback_percent: usize,
    ) -> Self {
        OrderChunk {
            txs,
            optional,
            kickback_percent,
        }
    }

    pub fn chunks_from_inner_share_bundle(inner: &ShareBundleInner) -> Vec<Self> {
        let total_refund_percent = inner.refund.iter().map(|r| r.percent).sum::<usize>();

        let mut accumulated_chunks = Vec::new();

        let mut prev_element_payed_refund = false;
        let mut current_chunk_txs = Vec::new();

        let release_chunk = |current_chunk_txs: &mut Vec<(B256, TxRevertBehavior)>,
                             accumulated_chunks: &mut Vec<OrderChunk>,
                             kickback_percent| {
            if !current_chunk_txs.is_empty() {
                accumulated_chunks.push(OrderChunk {
                    txs: std::mem::take(current_chunk_txs),
                    optional: inner.can_skip,
                    kickback_percent,
                })
            }
        };

        for (idx, body) in inner.body.iter().enumerate() {
            let current_element_pays_refund = !inner.refund.iter().any(|r| r.body_idx == idx);

            if prev_element_payed_refund != current_element_pays_refund {
                let chunk_refund_percent = if prev_element_payed_refund {
                    total_refund_percent
                } else {
                    0
                };
                release_chunk(
                    &mut current_chunk_txs,
                    &mut accumulated_chunks,
                    chunk_refund_percent,
                );
                prev_element_payed_refund = current_element_pays_refund;
            }

            match body {
                ShareBundleBody::Tx(tx) => {
                    current_chunk_txs.push((tx.hash(), tx.revert_behavior));
                }
                ShareBundleBody::Bundle(inner_bundle) => {
                    let chunk_refund_percent = if prev_element_payed_refund {
                        total_refund_percent
                    } else {
                        0
                    };
                    release_chunk(
                        &mut current_chunk_txs,
                        &mut accumulated_chunks,
                        chunk_refund_percent,
                    );

                    let mut inner_chunks = Self::chunks_from_inner_share_bundle(inner_bundle);
                    for chunk in &mut inner_chunks {
                        if current_element_pays_refund {
                            chunk.kickback_percent = multiply_inner_refunds(
                                chunk.kickback_percent,
                                chunk_refund_percent,
                            );
                        }
                        if inner.can_skip {
                            chunk.optional = true;
                        }
                    }
                    accumulated_chunks.extend(inner_chunks);
                }
            }
        }

        let chunk_refund_percent = if prev_element_payed_refund {
            total_refund_percent
        } else {
            0
        };
        release_chunk(
            &mut current_chunk_txs,
            &mut accumulated_chunks,
            chunk_refund_percent,
        );

        accumulated_chunks
    }
}

fn multiply_inner_refunds(a: usize, b: usize) -> usize {
    if a > 100 || b > 100 {
        return 0;
    }
    100 - (100 - a) * (100 - b) / 100
}

/// ExecutedBlockTx is data from the tx executed in the block
#[derive(Debug)]
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
}

#[derive(Debug, Clone, thiserror::Error, Eq, PartialEq)]
pub enum OrderIdentificationError {
    #[error("Tx not found: {0}")]
    TxNotFound(B256),
    #[error("Tx reverted: {0}")]
    TxReverted(B256),
    #[error("Tx is in incorrect position")]
    TxIsIncorrectPosition,
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
    ) -> Self {
        LandedOrderData {
            order,
            total_coinbase_profit,
            unique_coinbase_profit,
            error,
            overlapping_txs,
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
                    LandedOrderData::new(order.id, I256::ZERO, I256::ZERO, Some(e), Vec::new()),
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
            ));
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
    }

    result
}

#[derive(Debug)]
struct FoundChunkData {
    txs: Vec<B256>,
    last_tx_idx: usize,
}

fn find_order_chunk(
    block_data: &ExecutedBlockData,
    chunk: &OrderChunk,
) -> Result<FoundChunkData, OrderIdentificationError> {
    let mut txs = Vec::new();
    let mut last_tx_idx = 0;

    for (tx, revert) in &chunk.txs {
        let (idx, tx_data) = if let Some((idx, tx_data)) = block_data.find_tx(*tx) {
            (idx, tx_data)
        } else {
            // tx was not found in the block
            if revert != &TxRevertBehavior::AllowedExcluded {
                // tx not found
                return Err(OrderIdentificationError::TxNotFound(*tx));
            } else {
                continue;
            }
        };

        if !tx_data.success && !revert.can_revert() {
            return Err(OrderIdentificationError::TxReverted(*tx));
        }
        if idx < last_tx_idx {
            if revert != &TxRevertBehavior::AllowedExcluded {
                return Err(OrderIdentificationError::TxIsIncorrectPosition);
            } else {
                continue;
            }
        }
        last_tx_idx = idx;
        txs.push(*tx);
    }

    Ok(FoundChunkData { txs, last_tx_idx })
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
    let mut landed_txs = Vec::new();

    let mut idx_past_last_chunk = 0;
    for chunk in &order.chunks {
        match find_order_chunk(block_data, chunk) {
            Ok(data) => {
                // we found the chunk
                if data.last_tx_idx < idx_past_last_chunk {
                    // chunks are messed up, order not found
                    return Err(OrderIdentificationError::TxIsIncorrectPosition);
                }
                landed_txs.extend(data.txs.into_iter().map(|tx| (tx, chunk.kickback_percent)));
                idx_past_last_chunk = data.last_tx_idx + 1;
            }
            Err(e) => {
                if chunk.optional {
                    continue;
                }
                return Err(e);
            }
        }
    }

    if landed_txs.is_empty() {
        return Err(OrderIdentificationError::NoOrderTxs);
    }

    Ok(FoundOrderData { landed_txs })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        primitives::{Bundle, MempoolTx, Refund, ShareBundle, ShareBundleTx},
        utils::test_utils::*,
    };

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
                .unwrap_or_else(|| panic!("Order not found: {:?}", expected_result));
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
                vec![OrderChunk::new(
                    vec![(hash(2), TxRevertBehavior::NotAllowed)],
                    false,
                    0,
                )],
            ),
            // bundle 2 with 2/3 landed txs
            SimplifiedOrder::new(
                order_id(0xb2),
                vec![OrderChunk::new(
                    vec![
                        (hash(3), TxRevertBehavior::AllowedIncluded),
                        (hash(33), TxRevertBehavior::NotAllowed),
                        (hash(333), TxRevertBehavior::AllowedExcluded), // this tx never landed
                    ],
                    false,
                    0,
                )],
            ),
            // bundle with simple kickback
            SimplifiedOrder::new(
                order_id(0xb3),
                vec![
                    OrderChunk::new(vec![(hash(5), TxRevertBehavior::AllowedIncluded)], false, 0),
                    OrderChunk::new(vec![(hash(6), TxRevertBehavior::NotAllowed)], false, 90),
                ],
            ),
        ];

        let results = vec![
            // bundle 1 with 1 tx
            LandedOrderData::new(order_id(0xb1), i256(12), i256(12), None, vec![]),
            // bundle 2 with 2/3 landed txs
            LandedOrderData::new(order_id(0xb2), i256(13 + 14), i256(13 + 14), None, vec![]),
            // bundle with simple kickback
            LandedOrderData::new(order_id(0xb3), i256(15 + 2), i256(15 + 2), None, vec![]),
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
                vec![OrderChunk::new(
                    vec![
                        (hash(0x11), TxRevertBehavior::NotAllowed),
                        (hash(0xaa), TxRevertBehavior::NotAllowed),
                        (hash(0x12), TxRevertBehavior::NotAllowed),
                    ],
                    false,
                    0,
                )],
            ),
            SimplifiedOrder::new(
                order_id(0xb2),
                vec![OrderChunk::new(
                    vec![
                        (hash(0x21), TxRevertBehavior::NotAllowed),
                        (hash(0xaa), TxRevertBehavior::NotAllowed),
                        (hash(0x22), TxRevertBehavior::NotAllowed),
                    ],
                    false,
                    0,
                )],
            ),
        ];

        let results = vec![
            LandedOrderData::new(
                order_id(0xb1),
                i256(0x11 + 0xaa + 0x12),
                i256(0x11 + 0x12),
                None,
                vec![(order_id(0xb2), hash(0xaa))],
            ),
            LandedOrderData::new(
                order_id(0xb2),
                i256(0x21 + 0xaa + 0x22),
                i256(0x21 + 0x22),
                None,
                vec![(order_id(0xb1), hash(0xaa))],
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
                vec![OrderChunk::new(
                    vec![(hash(0x00), TxRevertBehavior::NotAllowed)],
                    false,
                    0,
                )],
            ),
            SimplifiedOrder::new(
                order_id(0xb2),
                vec![
                    OrderChunk::new(vec![(hash(0x00), TxRevertBehavior::NotAllowed)], false, 0),
                    OrderChunk::new(vec![(hash(0x02), TxRevertBehavior::NotAllowed)], false, 90),
                ],
            ),
            SimplifiedOrder::new(
                order_id(0xb3),
                vec![
                    OrderChunk::new(
                        vec![(hash(0x00), TxRevertBehavior::AllowedExcluded)],
                        true,
                        0,
                    ), // this is how we merge backruns now
                    OrderChunk::new(vec![(hash(0x03), TxRevertBehavior::NotAllowed)], false, 80),
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
            ),
            LandedOrderData::new(
                order_id(0xb2),
                i256(12 + 100),
                i256(100),
                None,
                vec![(order_id(0xb1), hash(0x00)), (order_id(0xb3), hash(0x00))],
            ),
            LandedOrderData::new(
                order_id(0xb3),
                i256(12 + 400),
                i256(400),
                None,
                vec![(order_id(0xb1), hash(0x00)), (order_id(0xb2), hash(0x00))],
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
                vec![OrderChunk::new(
                    vec![(hash(0x01), TxRevertBehavior::NotAllowed)],
                    false,
                    0,
                )],
            ),
            SimplifiedOrder::new(
                order_id(0xb2),
                vec![
                    OrderChunk::new(vec![(hash(0x02), TxRevertBehavior::NotAllowed)], false, 0),
                    OrderChunk::new(vec![(hash(0xAA), TxRevertBehavior::NotAllowed)], false, 90),
                ],
            ),
            SimplifiedOrder::new(
                order_id(0xb3),
                vec![OrderChunk::new(
                    vec![
                        (hash(0x03), TxRevertBehavior::NotAllowed),
                        (hash(0x02), TxRevertBehavior::NotAllowed),
                    ],
                    false,
                    0,
                )],
            ),
            SimplifiedOrder::new(
                order_id(0xb4),
                vec![
                    OrderChunk::new(vec![(hash(0x03), TxRevertBehavior::NotAllowed)], true, 0), // this is how we merge backruns now
                    OrderChunk::new(vec![(hash(0x02), TxRevertBehavior::NotAllowed)], false, 80),
                ],
            ),
            SimplifiedOrder::new(
                order_id(0xb5),
                vec![OrderChunk::new(
                    vec![(hash(0x01), TxRevertBehavior::NotAllowed)],
                    true,
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
            ),
            LandedOrderData::new(
                order_id(0xb2),
                i256(0),
                i256(0),
                Some(OrderIdentificationError::TxNotFound(hash(0xAA))),
                vec![],
            ),
            LandedOrderData::new(
                order_id(0xb3),
                i256(0),
                i256(0),
                Some(OrderIdentificationError::TxIsIncorrectPosition),
                vec![],
            ),
            LandedOrderData::new(
                order_id(0xb4),
                i256(0),
                i256(0),
                Some(OrderIdentificationError::TxIsIncorrectPosition),
                vec![],
            ),
            LandedOrderData::new(
                order_id(0xb5),
                i256(0),
                i256(0),
                Some(OrderIdentificationError::NoOrderTxs),
                vec![],
            ),
        ];
        assert_result(executed_block, orders, results);
    }

    #[test]
    fn test_simplified_order_conversion_mempool_tx() {
        let order = Order::Tx(MempoolTx {
            tx_with_blobs: tx(0x01),
        });
        let expected = SimplifiedOrder::new(
            OrderId::Tx(hash(0x01)),
            vec![OrderChunk::new(
                vec![(hash(0x01), TxRevertBehavior::AllowedIncluded)],
                false,
                0,
            )],
        );

        let got = SimplifiedOrder::new_from_order(&order);
        assert_eq!(expected, got);
    }

    #[test]
    fn test_simplified_order_conversion_bundle() {
        let bundle = Order::Bundle(Bundle {
            block: 0,
            min_timestamp: None,
            max_timestamp: None,
            txs: vec![tx(0x01), tx(0x02)],
            reverting_tx_hashes: vec![hash(0x02)],
            hash: Default::default(),
            uuid: uuid::uuid!("00000000-0000-0000-0000-ffff00000002"),
            replacement_data: None,
            signer: None,
            metadata: Default::default(),
        });
        let expected = SimplifiedOrder::new(
            OrderId::Bundle(uuid::uuid!("00000000-0000-0000-0000-ffff00000002")),
            vec![OrderChunk::new(
                vec![
                    (hash(0x01), TxRevertBehavior::NotAllowed),
                    (hash(0x02), TxRevertBehavior::AllowedExcluded),
                ],
                false,
                0,
            )],
        );

        let got = SimplifiedOrder::new_from_order(&bundle);
        assert_eq!(expected, got);
    }

    #[test]
    fn test_simplified_order_conversion_share_bundle() {
        let bundle = Order::ShareBundle(ShareBundle {
            hash: hash(0xb1),
            block: 0,
            max_block: 0,
            inner_bundle: ShareBundleInner {
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
            signer: None,
            replacement_data: None,
            original_orders: vec![],
            metadata: Default::default(),
        });
        let expected = SimplifiedOrder::new(
            OrderId::ShareBundle(hash(0xb1)),
            vec![
                OrderChunk::new(
                    vec![
                        (hash(0x01), TxRevertBehavior::NotAllowed),
                        (hash(0x02), TxRevertBehavior::AllowedExcluded),
                    ],
                    false,
                    0,
                ),
                OrderChunk::new(
                    vec![(hash(0x03), TxRevertBehavior::AllowedIncluded)],
                    false,
                    60,
                ),
                OrderChunk::new(vec![(hash(0x11), TxRevertBehavior::NotAllowed)], true, 60),
                OrderChunk::new(vec![(hash(0x12), TxRevertBehavior::NotAllowed)], true, 68),
                OrderChunk::new(
                    vec![(hash(0x04), TxRevertBehavior::AllowedIncluded)],
                    false,
                    0,
                ),
            ],
        );

        let got = SimplifiedOrder::new_from_order(&bundle);
        assert_eq!(got, expected);
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
            vec![OrderChunk::new(
                vec![
                    (hash(0x11), TxRevertBehavior::AllowedExcluded),
                    (hash(0x12), TxRevertBehavior::AllowedExcluded),
                    (hash(0x13), TxRevertBehavior::NotAllowed),
                ],
                false,
                0,
            )],
        )];

        let results = vec![LandedOrderData::new(
            order_id(0xb1),
            i256(0x11 + 0x13),
            i256(0x11 + 0x13),
            None,
            vec![],
        )];
        assert_result(executed_block, orders, results);
    }
}
