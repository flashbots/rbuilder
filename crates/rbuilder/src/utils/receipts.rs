use ahash::HashMap;
use alloy_consensus::ReceiptWithBloom;
use alloy_eips::Encodable2718;
use alloy_primitives::{Bloom, B256};
use eth_sparse_mpt::v2::trie::{proof_store::ProofStore, Trie};
use reth::primitives::Receipt;
use reth_primitives::Log;
use reth_primitives_traits::proofs;

use crate::building::TransactionExecutionInfo;

#[derive(Debug, Clone, Default)]
pub struct BloomCache {
    logs: HashMap<Log, Bloom>,
    trie: Trie,
    buff: Vec<u8>,
    empty_proof_store: ProofStore,
}

/// Speed up bloom filter calculation for block finalization using caching.
pub fn calculate_receipt_root_and_block_logs_bloom(
    cache: &mut BloomCache,
    executed_tx_infos: &[TransactionExecutionInfo],
    fast_finalize: bool,
) -> (B256, Bloom) {
    let mut block_logs_bloom = Bloom::ZERO;
    let mut receipts_with_blooms = Vec::with_capacity(executed_tx_infos.len());
    for executed_tx_info in executed_tx_infos {
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

        block_logs_bloom.accrue_bloom(&current_receipt_bloom);

        receipts_with_blooms.push(ReceiptWithBloom {
            receipt,
            logs_bloom: current_receipt_bloom,
        });
    }

    let receipts_root = if fast_finalize {
        faster_calculate_receipt_root(cache, &receipts_with_blooms)
    } else {
        proofs::calculate_receipt_root(&receipts_with_blooms)
    };

    (receipts_root, block_logs_bloom)
}

fn faster_calculate_receipt_root(
    cache: &mut BloomCache,
    receipts: &[ReceiptWithBloom<&Receipt>],
) -> B256 {
    let trie = &mut cache.trie;
    trie.clear_empty();
    let val = &mut cache.buff;

    for (idx, receipt) in receipts.iter().enumerate() {
        let index = alloy_rlp::encode_fixed_size(&idx);

        val.clear();
        receipt.encode_2718(val);
        trie.insert(&index, val).unwrap();
    }
    trie.root_hash(true, &cache.empty_proof_store).unwrap()
}

#[derive(Debug, Clone, Default)]
pub struct TransactionRootCache {
    trie: Trie,
    buff: Vec<u8>,
    empty_proof_store: ProofStore,
}

pub fn calculate_transactions_root(
    cache: &mut TransactionRootCache,
    executed_tx_infos: &[TransactionExecutionInfo],
    faster_finalize: bool,
) -> B256 {
    if faster_finalize {
        let trie = &mut cache.trie;
        trie.clear_empty();
        let val = &mut cache.buff;
        for (idx, executed_tx_info) in executed_tx_infos.iter().enumerate() {
            let tx_with_blobs = &executed_tx_info.tx;
            let index = alloy_rlp::encode_fixed_size(&idx);

            val.clear();
            tx_with_blobs.encode_2718(val);
            trie.insert(&index, val).unwrap();
        }
        let res = trie.root_hash(true, &cache.empty_proof_store).unwrap();
        return res;
    }
    let txs = executed_tx_infos
        .iter()
        .map(|info| info.tx.internal_tx_unsecure())
        .collect::<Vec<_>>();
    proofs::calculate_transaction_root(&txs)
}

#[cfg(test)]
mod tests {
    use alloy_consensus::TxReceipt;
    use alloy_consensus::TxType;
    use alloy_primitives::{address, fixed_bytes};
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
                gas_used: 0,
                coinbase_profit: Default::default(),
            })
            .collect::<Vec<_>>();

        let mut cache = BloomCache::default();
        for fast_finalize in [false, true, true] {
            let (got_receipt_root, got_logs_bloom) = calculate_receipt_root_and_block_logs_bloom(
                &mut cache,
                &executed_tx_info,
                fast_finalize,
            );
            assert_eq!(expected_receipt_root, got_receipt_root);
            assert_eq!(expected_logs_bloom, got_logs_bloom);
        }
    }

    #[test]
    fn test_faster_tx_root() {
        let mut data = Vec::new();
        for i in 0..100u64 {
            data.push(TransactionExecutionInfo {
                tx: tx(i),
                receipt: Default::default(),
                gas_used: 0,
                coinbase_profit: Default::default(),
            });
        }

        let mut cache = TransactionRootCache::default();
        let expected = calculate_transactions_root(&mut cache, &data, false);

        for _ in 0..2 {
            let got = calculate_transactions_root(&mut cache, &data, true);
            assert_eq!(expected, got);
        }
    }
}
