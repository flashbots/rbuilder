use crate::{backtest::execute::BlockBacktestValue, utils::build_info::rbuilder_version};
use sqlx::{
    sqlite::SqliteConnectOptions, ConnectOptions, Connection, Executor, Row, SqliteConnection,
};
use std::path::Path;

const VERSION: i64 = 2;

/// Storage of simulations (mainly BlockBacktestValue) to be able to compare different versions of our code
/// Store via BacktestResultsStorage::store_backtest_results and load via BacktestResultsStorage::load_latest_backtest_result to compare.
#[derive(Debug)]
pub struct BacktestResultsStorage {
    conn: SqliteConnection,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct StoredBacktestResult {
    pub time: time::OffsetDateTime,
    pub rbuilder_version: String,
    pub backtest_result: BlockBacktestValue,
}

impl BacktestResultsStorage {
    pub async fn new_from_path(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let mut res = Self {
            conn: SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .connect()
                .await?,
        };
        res.create_tables().await?;
        Ok(res)
    }

    pub async fn new_from_memory() -> eyre::Result<Self> {
        let mut res = Self {
            conn: SqliteConnectOptions::new().connect().await?,
        };
        res.create_tables().await?;
        Ok(res)
    }

    async fn create_tables(&mut self) -> eyre::Result<()> {
        // check if db was initialized before
        let initialized = sqlx::query(
            r#"
            SELECT count(*) from sqlite_master WHERE type='table' AND name='backtest_results';
        "#,
        )
        .fetch_one(&mut self.conn)
        .await?
        .try_get::<i64, _>(0)?
            > 0;

        if initialized {
            // check version
            let version = sqlx::query(
                r#"
                SELECT version from version;
            "#,
            )
            .fetch_optional(&mut self.conn)
            .await
            .ok()
            .flatten()
            .and_then(|row| row.try_get::<i64, _>(0).ok());
            if version != Some(VERSION) {
                return Err(eyre::eyre!(
                    "Invalid backtest result database, need {}, got {:?}. Please recreate db",
                    VERSION,
                    version
                ));
            }
            return Ok(());
        }

        self.conn
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS backtest_results (
                block INTEGER NOT NULL,
                timestamp INTEGER NOT NULL,
                rbuilder_version TEXT NOT NULL,
                backtest_result TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS version (
                version INTEGER NOT NULL
            );
            "#,
            )
            .await?;

        sqlx::query(
            "INSERT INTO version (version) SELECT ? WHERE NOT EXISTS(SELECT * FROM version);",
        )
        .bind(VERSION)
        .execute(&mut self.conn)
        .await?;

        Ok(())
    }

    pub async fn store_backtest_results(
        &mut self,
        time: time::OffsetDateTime,
        value: &[BlockBacktestValue],
    ) -> eyre::Result<()> {
        let mut tx = self.conn.begin().await?;
        let rbuilder_version = rbuilder_version().git_commit;
        for value in value {
            let _backtest_result = serde_json::to_value(value)?;
            sqlx::query(
                r#"
                INSERT INTO backtest_results (block, timestamp, rbuilder_version, backtest_result)
                VALUES (?, ?, ?, ?);
            "#,
            )
            .bind(value.block_number as i64)
            .bind(time.unix_timestamp())
            .bind(&rbuilder_version)
            .bind(serde_json::to_string(value)?)
            .execute(tx.as_mut())
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn load_latest_backtest_result(
        &mut self,
        block: u64,
    ) -> eyre::Result<Option<StoredBacktestResult>> {
        let res = sqlx::query(
            r#"
            SELECT timestamp, rbuilder_version, backtest_result
            FROM backtest_results
            WHERE block = ?
            ORDER BY timestamp DESC
            LIMIT 1;
        "#,
        )
        .bind(block as i64)
        .fetch_optional(&mut self.conn)
        .await?
        .map(|row| {
            let timestamp = row.try_get::<i64, _>(0)?;
            let rbuilder_version = row.try_get::<String, _>(1)?;
            let backtest_result = serde_json::from_str(row.try_get::<String, _>(2)?.as_str())?;
            Ok::<_, eyre::Report>(StoredBacktestResult {
                time: time::OffsetDateTime::from_unix_timestamp(timestamp)?,
                rbuilder_version,
                backtest_result,
            })
        })
        .transpose()?;
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::execute::BacktestBuilderOutput;

    use crate::utils::test_utils::*;

    #[tokio::test]
    async fn test_store_backtest_result() {
        let backtest_value = BlockBacktestValue {
            block_number: 13,
            winning_bid_value: u256(17),
            simulated_orders_count: 19,
            simulated_total_gas: 1000,
            filtered_orders_blocklist_count: 21,
            simulated_orders_with_refund: 22,
            simulated_refunds_paid: u256(23),
            extra_data: "extra".to_string(),
            builder_outputs: vec![BacktestBuilderOutput {
                orders_included: 7,
                builder_name: "builder".to_string(),
                our_bid_value: u256(19),
                included_orders: vec![order_id(1)],
                included_order_profits: vec![u256(100)],
            }],
        };

        let time = time::OffsetDateTime::from_unix_timestamp(
            time::OffsetDateTime::now_utc().unix_timestamp(),
        )
        .unwrap();

        let mut storage = BacktestResultsStorage::new_from_memory()
            .await
            .expect("create db");
        storage
            .store_backtest_results(time, std::slice::from_ref(&backtest_value))
            .await
            .unwrap();
        let res = storage
            .load_latest_backtest_result(13)
            .await
            .expect("write db");
        assert_eq!(
            res,
            Some(StoredBacktestResult {
                time,
                rbuilder_version: rbuilder_version().git_commit,
                backtest_result: backtest_value
            })
        );
    }
}
