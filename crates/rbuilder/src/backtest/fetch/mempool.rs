use crate::{
    backtest::OrdersWithTimestamp,
    primitives::{
        serialize::{RawOrder, RawTx, TxEncoding},
        Order,
    },
};
use mempool_dumpster::TransactionRangeError;
use sqlx::types::chrono::DateTime;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use tracing::error;

pub fn get_mempool_transactions(
    data_dir: impl AsRef<Path>,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> eyre::Result<Vec<OrdersWithTimestamp>> {
    let from_millis: i64 = (from.unix_timestamp_nanos() / 1_000_000).try_into()?;
    let to_millis: i64 = (to.unix_timestamp_nanos() / 1_000_000).try_into()?;

    check_and_download_transaction_files(from_millis, to_millis, &data_dir)?;

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

fn path_transactions(data_dir: impl AsRef<Path>, day: &str) -> PathBuf {
    data_dir
        .as_ref()
        .join(format!("transactions/{}.parquet", day))
}

fn check_and_download_transaction_files(
    from_millis: i64,
    to_millis: i64,
    data_dir: impl AsRef<Path>,
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
        let path = path_transactions(&data_dir, day);
        if !path.exists() {
            tracing::warn!("Missing file: {}", path.display());
            let config = mempool_dumpster::Config::new(&data_dir)
                .with_progress(true)
                .with_overwrite(true);
            config.download_transaction_file(day)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use time::macros::datetime;

    #[ignore]
    #[test]
    fn test_get_mempool_transactions() {
        let data_dir = std::env::var("MEMPOOL_DATADIR").expect("MEMPOOL_DATADIR not set");

        let from = datetime!(2023-09-04 23:59:00 UTC);
        let to = datetime!(2023-09-05 00:01:00 UTC);

        let txs = get_mempool_transactions(data_dir, from, to).unwrap();
        assert_eq!(txs.len(), 1938);
        dbg!(txs.len());
        dbg!(&txs[0]);
        dbg!(&txs[txs.len() - 1]);
    }
}
