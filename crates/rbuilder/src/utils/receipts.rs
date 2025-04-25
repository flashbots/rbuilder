use ahash::HashMap;
use alloy_consensus::{proofs::calculate_receipt_root, ReceiptWithBloom};
use alloy_primitives::{Bloom, B256};
use reth::primitives::Receipt;
use reth_primitives::Log;

pub type BloomCache = HashMap<Log, Bloom>;

/// Speed up bloom filter calculation for block finalization using caching.
pub fn calculate_receipt_root_and_block_logs_bloom(
    receipts: &[Receipt],
    cache: &mut BloomCache,
) -> (B256, Bloom) {
    let mut block_logs_bloom = Bloom::ZERO;

    let mut receipts_with_blooms = Vec::with_capacity(receipts.len());

    for receipt in receipts {
        let mut current_receipt_bloom = Bloom::ZERO;

        for log in &receipt.logs {
            let log_bloom = if let Some(log_bloom) = cache.get(log) {
                *log_bloom
            } else {
                let mut current_log_bloom = Bloom::ZERO;
                current_log_bloom.accrue_log(log);

                cache.insert(log.clone(), current_log_bloom);

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

    let receipts_root = calculate_receipt_root(&receipts_with_blooms);

    (receipts_root, block_logs_bloom)
}

#[cfg(test)]
mod tests {
    use alloy_consensus::TxReceipt;
    use alloy_consensus::TxType;
    use alloy_primitives::{address, fixed_bytes};
    use reth_primitives::{logs_bloom, Log, LogData};

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

        let mut cache = BloomCache::default();
        let (got_receipt_root, got_logs_bloom) =
            calculate_receipt_root_and_block_logs_bloom(&receipts, &mut cache);
        assert_eq!(expected_receipt_root, got_receipt_root);
        assert_eq!(expected_logs_bloom, got_logs_bloom);

        // call second time to check caching
        let (got_receipt_root, got_logs_bloom) =
            calculate_receipt_root_and_block_logs_bloom(&receipts, &mut cache);
        assert_eq!(expected_receipt_root, got_receipt_root);
        assert_eq!(expected_logs_bloom, got_logs_bloom);
    }
}
