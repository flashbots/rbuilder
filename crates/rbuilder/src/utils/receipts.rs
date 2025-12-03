use ahash::HashMap;
use alloy_consensus::ReceiptWithBloom;
use alloy_eips::Encodable2718;
use alloy_primitives::{Bloom, Bytes, B256};
use alloy_trie::{proof::ProofRetainer, root::adjust_index_for_rlp, HashBuilder, Nibbles};
use eth_sparse_mpt::v2::trie::{proof_store::ProofStore, Trie};
use itertools::Itertools;
use reth::primitives::Receipt;
use reth_primitives::Log;

use crate::building::TransactionExecutionInfo;

#[derive(Debug, Clone, Default)]
pub struct ReceiptsDataCache {
    logs: HashMap<Log, Bloom>,
    trie: Trie,
    buff: Vec<u8>,
    empty_proof_store: ProofStore,
    block_logs_bloom_without_last_tx: Bloom,
    receipts_bloom_without_last_tx: Vec<Bloom>,
}

#[derive(Debug, Clone)]
pub struct ReceiptsData {
    /// Receipts root for the block.
    pub receipts_root: B256,
    /// Logs bloom for the block.
    pub logs_bloom: Bloom,
    /// Logs bloom for the block before the last payment transaction.
    pub pre_payment_logs_bloom: Bloom,
    /// Merkle proof of the last receipt.
    /// Used for bid adjustments.
    pub placeholder_receipt_proof: Vec<Bytes>,
}

/// Speed up bloom filter calculation for block finalization using caching.
pub fn calculate_receipts_data(
    cache: &mut ReceiptsDataCache,
    executed_tx_infos: &[TransactionExecutionInfo],
    fast_finalize: bool,
    adjust_finalized_blocks: bool,
) -> ReceiptsData {
    let mut block_logs_bloom = Bloom::ZERO;
    let mut receipts_blooms = Vec::with_capacity(executed_tx_infos.len());
    if adjust_finalized_blocks {
        block_logs_bloom = cache.block_logs_bloom_without_last_tx;
        receipts_blooms.extend_from_slice(&cache.receipts_bloom_without_last_tx);
    } else {
        for executed_tx_info in executed_tx_infos.iter().take(executed_tx_infos.len() - 1) {
            let receipt = &executed_tx_info.receipt;
            let mut current_receipt_bloom = Bloom::ZERO;

            for log in &receipt.logs {
                let log_bloom = if let Some(log_bloom) = cache.logs.get(log) {
                    *log_bloom
                } else {
                    let mut current_log_bloom = Bloom::ZERO;
                    current_log_bloom.accrue_log(log);
                    cache.logs.insert(log.clone(), current_log_bloom);
                    current_log_bloom
                };
                current_receipt_bloom.accrue_bloom(&log_bloom);
            }
            receipts_blooms.push(current_receipt_bloom);

            block_logs_bloom.accrue_bloom(&current_receipt_bloom);
        }
        cache.block_logs_bloom_without_last_tx = block_logs_bloom;
        cache.receipts_bloom_without_last_tx = receipts_blooms.clone();
    }

    let pre_payment_logs_bloom = block_logs_bloom;
    {
        let executed_tx_info = executed_tx_infos.last().unwrap();
        let receipt = &executed_tx_info.receipt;
        let mut current_receipt_bloom = Bloom::ZERO;

        for log in &receipt.logs {
            let log_bloom = if let Some(log_bloom) = cache.logs.get(log) {
                *log_bloom
            } else {
                let mut current_log_bloom = Bloom::ZERO;
                current_log_bloom.accrue_log(log);
                cache.logs.insert(log.clone(), current_log_bloom);
                current_log_bloom
            };
            current_receipt_bloom.accrue_bloom(&log_bloom);
        }
        receipts_blooms.push(current_receipt_bloom);
        block_logs_bloom.accrue_bloom(&current_receipt_bloom);
    }

    let mut receipts_with_blooms = Vec::with_capacity(executed_tx_infos.len());
    for (info, logs_bloom) in executed_tx_infos.iter().zip(receipts_blooms.into_iter()) {
        receipts_with_blooms.push(ReceiptWithBloom {
            receipt: &info.receipt,
            logs_bloom,
        });
    }

    let (receipts_root, placeholder_receipt_proof) = if fast_finalize {
        calculate_receipts_root_and_placeholder_proof_with_cache(
            cache,
            &receipts_with_blooms,
            adjust_finalized_blocks,
        )
    } else {
        calculate_receipts_root_and_placeholder_proof_with_alloy(cache, &receipts_with_blooms)
    };

    ReceiptsData {
        logs_bloom: block_logs_bloom,
        pre_payment_logs_bloom,
        receipts_root,
        placeholder_receipt_proof,
    }
}

fn calculate_receipts_root_and_placeholder_proof_with_cache(
    cache: &mut ReceiptsDataCache,
    receipts: &[ReceiptWithBloom<&Receipt>],
    adjust_finalized_block: bool,
) -> (B256, Vec<Bytes>) {
    let trie = &mut cache.trie;
    let root = if !adjust_finalized_block {
        trie.clear_empty();
        for (idx, receipt) in receipts.iter().enumerate() {
            let index = alloy_rlp::encode_fixed_size(&idx);
            cache.buff.clear();
            receipt.encode_2718(&mut cache.buff);
            trie.insert(&index, &cache.buff).unwrap();
        }
        trie.root_hash(true, &cache.empty_proof_store).unwrap()
    } else {
        let idx = receipts.len() - 1;
        let index = alloy_rlp::encode_fixed_size(&idx);
        cache.buff.clear();
        receipts[idx].encode_2718(&mut cache.buff);
        trie.insert(&index, &cache.buff).unwrap();
        trie.root_hash(false, &cache.empty_proof_store).unwrap()
    };

    let target_idx = receipts.len().checked_sub(1).unwrap();
    let nibbles = Nibbles::unpack(alloy_rlp::encode_fixed_size(&target_idx));
    let proof_with_value = trie
        .get_proof_nibbles_key(&nibbles, &cache.empty_proof_store)
        .unwrap();
    let proof = proof_with_value
        .proof
        .into_iter()
        .map(|(_, n)| n.into())
        .collect();

    (root, proof)
}

fn calculate_receipts_root_and_placeholder_proof_with_alloy(
    cache: &mut ReceiptsDataCache,
    receipts: &[ReceiptWithBloom<&Receipt>],
) -> (B256, Vec<Bytes>) {
    let encoded_receipts = receipts
        .iter()
        .map(|receipt| {
            cache.buff.clear();
            receipt.encode_2718(&mut cache.buff);
            cache.buff.clone().into()
        })
        .collect::<Vec<_>>();
    let target_idx = receipts.len().checked_sub(1).unwrap();
    ordered_trie_root_and_proof(&encoded_receipts, target_idx)
}

#[derive(Debug, Clone, Default)]
pub struct TransactionRootCache {
    trie: Trie,
    buff: Vec<u8>,
    empty_proof_store: ProofStore,
}

/// Calculate transactions root and proof for the last transactions
pub fn calculate_tx_root_and_placeholder_proof(
    cache: &mut TransactionRootCache,
    executed_tx_infos: &[TransactionExecutionInfo],
    faster_finalize: bool,
    prefinalized: bool,
) -> (B256, Vec<Bytes>) {
    if faster_finalize {
        calculate_tx_root_and_placeholder_proof_with_cache(cache, executed_tx_infos, prefinalized)
    } else {
        calculate_tx_root_and_placeholder_proof_with_alloy(cache, executed_tx_infos)
    }
}

/// Calculate transaction root and placeholder proof using cached trie.
fn calculate_tx_root_and_placeholder_proof_with_cache(
    cache: &mut TransactionRootCache,
    executed_tx_infos: &[TransactionExecutionInfo],
    adjust_finalized_block: bool,
) -> (B256, Vec<Bytes>) {
    let trie = &mut cache.trie;
    let val = &mut cache.buff;

    let root = if !adjust_finalized_block {
        trie.clear_empty();
        for (idx, executed_tx_info) in executed_tx_infos.iter().enumerate() {
            let tx_with_blobs = &executed_tx_info.tx;
            let index = alloy_rlp::encode_fixed_size(&idx);

            val.clear();
            tx_with_blobs.encode_2718(val);
            trie.insert(&index, val).unwrap();
        }
        trie.root_hash(true, &cache.empty_proof_store).unwrap()
    } else {
        let idx = executed_tx_infos.len() - 1;
        let tx_with_blobs = &executed_tx_infos[idx].tx;
        let index = alloy_rlp::encode_fixed_size(&idx);

        val.clear();
        tx_with_blobs.encode_2718(val);
        trie.insert(&index, val).unwrap();
        trie.root_hash(false, &cache.empty_proof_store).unwrap()
    };

    let target_idx = executed_tx_infos.len().checked_sub(1).unwrap();
    let nibbles = Nibbles::unpack(alloy_rlp::encode_fixed_size(&target_idx));
    let proof_with_value = trie
        .get_proof_nibbles_key(&nibbles, &cache.empty_proof_store)
        .unwrap();
    let proof = proof_with_value
        .proof
        .into_iter()
        .map(|(_, n)| n.into())
        .collect();

    (root, proof)
}

/// Calculate transaction root and placeholder proof using alloy.
fn calculate_tx_root_and_placeholder_proof_with_alloy(
    cache: &mut TransactionRootCache,
    executed_tx_infos: &[TransactionExecutionInfo],
) -> (B256, Vec<Bytes>) {
    let encoded_txs = executed_tx_infos
        .iter()
        .map(|info| {
            let tx = info.tx.internal_tx_unsecure();
            cache.buff.clear();
            tx.encode_2718(&mut cache.buff);
            cache.buff.clone().into()
        })
        .collect::<Vec<_>>();
    let target_idx = executed_tx_infos.len().checked_sub(1).unwrap();
    ordered_trie_root_and_proof(&encoded_txs, target_idx)
}

/// Compute trie root of the collection of items and proof for the target element.
pub fn ordered_trie_root_and_proof(items: &[Bytes], proof_index: usize) -> (B256, Vec<Bytes>) {
    let items_len = items.len();

    let proof_target_encoded = alloy_rlp::encode_fixed_size(&proof_index);
    let proof_retainer = ProofRetainer::from_iter([Nibbles::unpack(&proof_target_encoded)]);

    let mut hb = HashBuilder::default().with_proof_retainer(proof_retainer);
    for i in 0..items_len {
        let index = adjust_index_for_rlp(i, items_len);
        let index_encoded = alloy_rlp::encode_fixed_size(&index);
        hb.add_leaf(Nibbles::unpack(&index_encoded), &items[index]);
    }

    let root = hb.root();

    let proof_nodes = hb.take_proof_nodes();
    let proof = proof_nodes
        .into_inner()
        .into_iter()
        .sorted_unstable_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, node)| node)
        .collect();

    (root, proof)
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{TxReceipt, TxType};
    use alloy_primitives::{address, fixed_bytes};
    use rbuilder_primitives::BlockSpace;
    use reth_primitives::{logs_bloom, Log, LogData};

    use crate::utils::test_utils::tx;

    use super::*;

    #[test]
    fn test_cached_blooms() {
        let receipts = vec![
            Receipt {
                tx_type: TxType::Eip1559,
                success: true,
                cumulative_gas_used: 1000,
                logs: vec![
                    Log {
                        address: address!("87179882e0F1C1F99c585A8eE12d60eA0c89bc0C"),
                        data: LogData::new_unchecked(
                            vec![fixed_bytes!(
                                "5aeac5d808a2f7646502234d71ead4d4c0fea41ad8d015b46b8c6db262fdbbee"
                            )],
                            Default::default(),
                        ),
                    },
                    Log {
                        address: address!("87179882e0F1C1F99c585A8eE12d60eA0c89bc0C"),
                        data: LogData::new_unchecked(vec![], Default::default()),
                    },
                ],
            },
            Receipt {
                tx_type: TxType::Eip4844,
                success: false,
                cumulative_gas_used: 2000,
                logs: vec![Log {
                    address: address!("8E1f4CbAe96647baac384124537ff7CD8e503DEC"),
                    data: LogData::new_unchecked(vec![], Default::default()),
                }],
            },
            Receipt {
                tx_type: TxType::Eip2930,
                success: false,
                cumulative_gas_used: 3000,
                logs: vec![Log {
                    address: address!("87179882e0F1C1F99c585A8eE12d60eA0c89bc0C"),
                    data: LogData::new_unchecked(
                        vec![
                            fixed_bytes!(
                                "5aeac5d808a2f7646502234d71ead4d4c0fea41ad8d015b46b8c6db262fdbbee"
                            ),
                            fixed_bytes!(
                                "6e3998bc71f04fd0e13216663edad9293abbac1e552ba5118584f9a709c8ce32"
                            ),
                            fixed_bytes!(
                                "05cdbda6faff1f78c9d22d4bd461527a032d091a5c2e96dcbb131cbb53d58cb8"
                            ),
                        ],
                        Default::default(),
                    ),
                }],
            },
        ];

        let expected_receipt_root = Receipt::calculate_receipt_root_no_memo(&receipts);
        let expected_logs_bloom = logs_bloom(receipts.iter().flat_map(|r| r.logs()));

        let executed_tx_info = receipts
            .into_iter()
            .map(|receipt| TransactionExecutionInfo {
                tx: tx(1),
                receipt,
                space_used: BlockSpace::ZERO,
                coinbase_profit: Default::default(),
            })
            .collect::<Vec<_>>();

        let mut cache = ReceiptsDataCache::default();
        for fast_finalize in [false, true, true] {
            let got_receipts_data =
                calculate_receipts_data(&mut cache, &executed_tx_info, fast_finalize, false);
            assert_eq!(expected_receipt_root, got_receipts_data.receipts_root);
            assert_eq!(expected_logs_bloom, got_receipts_data.logs_bloom);
        }
    }

    #[test]
    fn test_faster_tx_root() {
        let mut data = Vec::new();
        for i in 0..100u64 {
            data.push(TransactionExecutionInfo {
                tx: tx(i),
                receipt: Default::default(),
                space_used: BlockSpace::ZERO,
                coinbase_profit: Default::default(),
            });
        }

        let mut cache = TransactionRootCache::default();
        let expected = calculate_tx_root_and_placeholder_proof(&mut cache, &data, false, false);

        for _ in 0..2 {
            let got = calculate_tx_root_and_placeholder_proof(&mut cache, &data, true, false);
            assert_eq!(expected, got);
        }
    }
}
