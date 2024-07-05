use crate::{
    backtest::{
        fetch::datasource::{BlockRef, Datasource},
        OrdersWithTimestamp,
    },
    primitives::{
        serialize::{RawOrder, RawTx, TxEncoding},
        Order,
    },
};
use async_trait::async_trait;
use eyre::WrapErr;
use mempool_dumpster::TransactionRangeError;
use sqlx::types::chrono::DateTime;
use std::{
    fs::create_dir_all,
    path::{Path, PathBuf},
};
use time::{Duration, OffsetDateTime};
use tracing::{error, trace};

pub fn get_mempool_transactions(
    data_dir: &Path,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> eyre::Result<Vec<OrdersWithTimestamp>> {
    let from_millis: i64 = (from.unix_timestamp_nanos() / 1_000_000).try_into()?;
    let to_millis: i64 = (to.unix_timestamp_nanos() / 1_000_000).try_into()?;

    check_and_download_transaction_files(from_millis, to_millis, data_dir)?;

    let txs = mempool_dumpster::get_raw_transactions(data_dir, from_millis, to_millis)?;
    Ok(txs
        .into_iter()
        .filter_map(|tx| {
            let order: Order = RawOrder::Tx(RawTx {
                tx: tx.raw_tx.into(),
            })
            .decode(TxEncoding::NoBlobData)
            .map_err(|err| error!("Failed to parse raw tx: {:?}", err))
            .ok()?;
            let timestamp_ms = tx
                .timestamp_ms
                .try_into()
                .map_err(|err| error!("Failed to parse timestamp: {:?}", err))
                .ok()?;

            Some(OrdersWithTimestamp {
                timestamp_ms,
                order,
                sim_value: None,
            })
        })
        .collect())
}

fn path_transactions(data_dir: &Path, day: &str) -> PathBuf {
    data_dir.join(format!("transactions/{}.parquet", day))
}

fn check_and_download_transaction_files(
    from_millis: i64,
    to_millis: i64,
    data_dir: &Path,
) -> eyre::Result<()> {
    let from_time = DateTime::from_timestamp_millis(from_millis)
        .ok_or(TransactionRangeError::InvalidTimestamp)?;
    let to_time = DateTime::from_timestamp_millis(to_millis)
        .ok_or(TransactionRangeError::InvalidTimestamp)?;

    // get all days in range
    let mut days = Vec::new();
    let mut current_day = from_time.date_naive();
    while current_day <= to_time.date_naive() {
        days.push(current_day.format("%Y-%m-%d").to_string());
        current_day = current_day
            .succ_opt()
            .ok_or(TransactionRangeError::InvalidTimestamp)?;
    }

    // check all day files
    for day in &days {
        let path = path_transactions(data_dir, day);
        if !path.exists() {
            tracing::warn!("Missing file: {}", path.display());
            let config = mempool_dumpster::Config::new(data_dir)
                .with_progress(true)
                .with_overwrite(true);
            config.download_transaction_file(day)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct MempoolDumpsterDatasource {
    path: PathBuf,
}

#[async_trait]
impl Datasource for MempoolDumpsterDatasource {
    async fn get_data(&self, block: BlockRef) -> eyre::Result<Vec<OrdersWithTimestamp>> {
        let (from, to) = {
            let block_time = OffsetDateTime::from_unix_timestamp(block.block_timestamp as i64)?;
            (
                block_time - Duration::minutes(3),
                // we look ahead by 5 seconds in case block bid was delayed relative to the timestamp
                block_time + Duration::seconds(5),
            )
        };
        let mempool_txs = get_mempool_transactions(&self.path, from, to).wrap_err_with(|| {
            format!(
                "Failed to fetch mempool transactions for block {}",
                block.block_number,
            )
        })?;
        trace!(
            "Fetched unfiltered mempool transactions, count: {}",
            mempool_txs.len()
        );
        // TODO: Filter to only include tnxs from block?
        Ok(mempool_txs)
    }

    fn clone_box(&self) -> Box<dyn Datasource> {
        Box::new(self.clone())
    }
}

impl MempoolDumpsterDatasource {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, std::io::Error> {
        let path: PathBuf = path.into();

        // create the directory if it doesn't exist
        create_dir_all(&path)?;
        create_dir_all(path.join("transactions"))?;

        Ok(Self { path })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use time::macros::datetime;

    #[tokio::test]
    async fn test_get_mempool_transactions() {
        let data_dir = match std::env::var("MEMPOOL_DATADIR") {
            Ok(dir) => dir,
            Err(_) => {
                println!("MEMPOOL_DATADIR not set, skipping test");
                return;
            }
        };

        let source = MempoolDumpsterDatasource::new(data_dir).unwrap();
        let block = BlockRef {
            block_number: 18048817,
            block_timestamp: datetime!(2023-09-04 23:59:00 UTC).unix_timestamp() as u64,
        };

        let txs = source.get_data(block).await.unwrap();
        assert_eq!(txs.len(), 1732);
    }
}
