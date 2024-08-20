use crate::backtest::BuiltBlockData;
use crate::primitives::Order;
use crate::{
    backtest::{
        fetch::data_source::{BlockRef, DataSource},
        OrdersWithTimestamp,
    },
    primitives::{Bundle, TransactionSignedEcRecoveredWithBlobs},
};
use alloy_rlp::Decodable;
use async_trait::async_trait;
use csv::Reader;
use eyre::Context;
use reth::primitives::TransactionSignedEcRecovered;
use revm_primitives::B256;
use std::{collections::HashMap, fs::File, path::PathBuf};
use tracing::trace;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct CSVDatasource {
    batches: HashMap<u64, Vec<TransactionSignedEcRecovered>>,
}

impl CSVDatasource {
    pub fn new(filename: impl Into<PathBuf>) -> eyre::Result<Self> {
        let batches = Self::load_transactions_from_csv(filename.into())?;
        Ok(Self { batches })
    }

    fn load_transactions_from_csv(
        filename: PathBuf,
    ) -> eyre::Result<HashMap<u64, Vec<TransactionSignedEcRecovered>>> {
        let file = File::open(&filename)
            .wrap_err_with(|| format!("Failed to open file: {}", filename.display()))?;
        let mut reader = Reader::from_reader(file);
        let mut batches: HashMap<u64, Vec<TransactionSignedEcRecovered>> = HashMap::new();

        for result in reader.records() {
            let record = result?;
            if record.len() != 2 {
                return Err(eyre::eyre!("Invalid CSV format"));
            }

            let batch_number: u64 = record[0].parse()?;
            let rlp_hex = &record[1];
            let rlp_bytes = hex::decode(rlp_hex)?;
            let tx = TransactionSignedEcRecovered::decode(&mut &rlp_bytes[..])?;

            batches.entry(batch_number % 10).or_default().push(tx);
        }

        Ok(batches)
    }
}

#[async_trait]
impl DataSource for CSVDatasource {
    async fn get_orders(&self, block: BlockRef) -> eyre::Result<Vec<OrdersWithTimestamp>> {
        // The csv datasource is one with 10 batches, where batch is a list of transactions
        // Since we don't have full "real" blocks, we'll just use the block number to determine the batch
        // Thus the usage of mod 10 is just to determine the batch number that we get transactions from, e.g. block 100 corresponds to 0, 101 to 1, 109 to 9, etc.
        let batch_number = block.block_number % 10;
        let transactions = self.batches.get(&batch_number).cloned().unwrap_or_default();

        let mut uuid_num = 0;
        let orders: Vec<OrdersWithTimestamp> = transactions
            .into_iter()
            .map(|tx| {
                let order = transaction_to_order(block.block_number, &mut uuid_num, tx);
                OrdersWithTimestamp {
                    timestamp_ms: block.block_timestamp,
                    order,
                    sim_value: None,
                }
            })
            .collect();

        trace!(
            "Fetched synthetic transactions from CSV for block {}, batch {}, count: {}",
            block.block_number,
            batch_number,
            orders.len()
        );

        Ok(orders)
    }

    fn clone_box(&self) -> Box<dyn DataSource> {
        Box::new(self.clone())
    }

    async fn get_built_block_data(
        &self,
        _block_hash: B256,
    ) -> eyre::Result<Option<BuiltBlockData>> {
        panic!("get_built_block_data not implemented for cvs");
    }
}

fn transaction_to_order(
    block: u64,
    uuid_num: &mut u128,
    tx: TransactionSignedEcRecovered,
) -> Order {
    let uuid_bytes = uuid_num.to_be_bytes();
    let tx_with_blobs = TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx).unwrap();
    let bundle = Bundle {
        txs: vec![tx_with_blobs.clone()],
        hash: tx_with_blobs.hash(),
        reverting_tx_hashes: vec![],
        block,
        uuid: Uuid::from_bytes(uuid_bytes),
        min_timestamp: None,
        max_timestamp: None,
        replacement_data: None,
        signer: None,
        metadata: Default::default(),
    };
    *uuid_num += 1;
    Order::Bundle(bundle)
}
