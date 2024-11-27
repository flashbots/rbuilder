// store orders in the sqlite database

use crate::{
    backtest::{BlockData, BuiltBlockData, OrdersWithTimestamp, RawOrdersWithTimestamp},
    mev_boost::BuilderBlockReceived,
    primitives::{
        serialize::{RawOrder, TxEncoding},
        OrderId,
    },
    utils::timestamp_ms_to_offset_datetime,
};
use ahash::{HashMap, HashSet};
use alloy_primitives::{
    utils::{format_ether, parse_ether, ParseUnits, Unit},
    Address, B256, I256, U256,
};
use lz4_flex::{block::DecompressError, compress_prepend_size, decompress_size_prepended};
use rayon::prelude::*;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteRow},
    ConnectOptions, Connection, Executor, Row, SqliteConnection,
};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    str::FromStr,
};

/// Version of the data/format on the DB.
/// Since we don't have backwards compatibility every time this is increased we must re-create the DB (manually delete the sqlite)
const VERSION: i64 = 10;

/// Storage of BlockData.
/// It allows us to locally cache (using a SQLite DB) all the info we need for backtesting so we don't have to
/// go to the mempool dumpster (or any other source) every time we simulate a block.
pub struct HistoricalDataStorage {
    conn: SqliteConnection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalBlocksInfo {
    pub block_number: u64,
    pub block_hash: B256,
    pub fee_recipient: Address,
    pub bid_value: U256,
    pub order_count: usize,
}

impl HistoricalDataStorage {
    pub async fn new_from_path(path: impl AsRef<Path>) -> eyre::Result<Self> {
        // expand the path
        let path_expanded = shellexpand::tilde(path.as_ref().to_str().unwrap());
        let path_expanded_buf = PathBuf::from(OsString::from(path_expanded.as_ref()));

        let mut res = Self {
            conn: SqliteConnectOptions::new()
                .filename(path_expanded_buf)
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

    pub async fn create_tables(&mut self) -> eyre::Result<()> {
        // check if db was initialized before
        let initialized = sqlx::query(
            r#"
            SELECT count(*) from sqlite_master WHERE type='table' AND name='orders';
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
                return Err(eyre::eyre!("Invalid local backtest database version, need {}, got {:?}. Please recreate backtest sqlite db", VERSION, version));
            }
            return Ok(());
        }

        self.conn
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS orders (
                block_number INTEGER NOT NULL,
                timestamp_ms INTEGER NOT NULL,
                order_type TEXT NOT NULL,
                coinbase_profit TEXT,
                gas_used INTEGER,
                order_id TEXT,
                order_data BLOB NOT NULL
            );

            CREATE INDEX IF NOT EXISTS orders_block_number_idx ON orders (block_number);

            CREATE TABLE IF NOT EXISTS blocks (
                block_number INTEGER NOT NULL,
                block_hash TEXT NOT NULL,
                fee_recipient TEXT NOT NULL,
                bid_value TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS blocks_block_number_idx ON blocks (block_number);

            CREATE TABLE IF NOT EXISTS blocks_data (
                block_number INTEGER NOT NULL,
                winning_bid_trace BLOB NOT NULL,
                onchain_block BLOB NOT NULL
            );


            CREATE TABLE IF NOT EXISTS built_block_included_orders (
                block_number INTEGER NOT NULL,
                order_id TEXT NOL NULL
            );

            CREATE TABLE IF NOT EXISTS built_block_data (
                block_number INTEGER NOT NULL,
                orders_closed_at_ts_ms INTEGER NOT NULL,
                sealed_at_ts_ms INTEGER NOT NULL,
                profit TEXT NOT NULL
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

    pub async fn write_block_data(&mut self, block_data: BlockData) -> eyre::Result<()> {
        self.conn.transaction(|conn| Box::pin(async move {
            // check if block is already in the database
            let block_exists = sqlx::query(
                r#"
            SELECT count(*) from blocks WHERE block_number = ?;
            "#,
            ).bind(block_data.block_number as i64)
                .fetch_one(conn.as_mut())
                .await?
                .try_get::<i64, _>(0)?
                > 0;
            if block_exists {
                // delete old data
                sqlx::query(
                    r#"
                DELETE FROM blocks WHERE block_number = ?;
                "#,
                ).bind(block_data.block_number as i64)
                    .execute(conn.as_mut())
                    .await?;
                sqlx::query(
                    r#"
                DELETE FROM orders WHERE block_number = ?;
                "#,
                ).bind(block_data.block_number as i64)
                    .execute(conn.as_mut())
                    .await?;
                sqlx::query(
                    r#"
                DELETE FROM blocks_data WHERE block_number = ?;
                "#,
                ).bind(block_data.block_number as i64)
                    .execute(conn.as_mut())
                    .await?;
                sqlx::query(
                    r#"
                DELETE FROM built_block_included_orders WHERE block_number = ?;
                "#,
                ).bind(block_data.block_number as i64)
                    .execute(conn.as_mut())
                    .await?;
                sqlx::query(
                    r#"
                DELETE FROM built_block_data WHERE block_number = ?;
                "#,
                ).bind(block_data.block_number as i64)
                    .execute(conn.as_mut())
                    .await?;
            }

            sqlx::query(
                r#"
            INSERT INTO blocks (block_number, block_hash, fee_recipient, bid_value)
            VALUES (?, ?, ?, ?)
            "#,
            ).bind(block_data.block_number as i64)
                .bind(format!("{:?}", block_data.winning_bid_trace.block_hash))
                .bind(format!("{:?}", block_data.winning_bid_trace.proposer_fee_recipient))
                .bind(format_ether(block_data.winning_bid_trace.value))
                .execute(conn.as_mut())
                .await?;

            let winning_bid_json = compress_data(&serde_json::to_vec(&block_data.winning_bid_trace)?);
            let onchain_block_json = compress_data(&serde_json::to_vec(&block_data.onchain_block)?);
            sqlx::query(
                r#"
            INSERT INTO blocks_data (block_number, winning_bid_trace, onchain_block)
            VALUES (?, ?, ?)
            "#,
            ).bind(block_data.block_number as i64)
                .bind(winning_bid_json)
                .bind(onchain_block_json)
                .execute(conn.as_mut())
                .await?;

            for order in block_data.available_orders {
                let raw_order: RawOrdersWithTimestamp = order.clone().into();
                let order_id = order.order.id().to_string();
                let order_json = compress_data(&serde_json::to_vec(&raw_order)?);
                sqlx::query(
                    r#"
                INSERT INTO orders (block_number, timestamp_ms, order_type, coinbase_profit, gas_used, order_id, order_data)
                VALUES (?, ?, ?, ?, ?, ?, ?)
                "#,
                ).bind(block_data.block_number as i64)
                    .bind(order.timestamp_ms as i64)
                    .bind(order_type(&raw_order.order))
                    .bind(order.sim_value.clone().map(|v| {
                        format_ether(v.coinbase_profit)
                    }))
                    .bind(order.sim_value.clone().map(|v| v.gas_used as i64))
                    .bind(order_id)
                    .bind(order_json)
                    .execute(conn.as_mut())
                    .await?;
            }

            if let Some(built_block_data) = block_data.built_block_data {
                for order_id in built_block_data.included_orders {
                    let order_id = order_id.to_string();
                    sqlx::query(
                        r#"
                    INSERT INTO built_block_included_orders (block_number, order_id)
                    VALUES (?, ?)
                    "#,
                    ).bind(block_data.block_number as i64)
                        .bind(order_id)
                        .execute(conn.as_mut())
                        .await?;
                }

                let orders_closed_at_ts_ms = built_block_data.orders_closed_at.unix_timestamp_nanos() as i64 / 1_000_000;
                let sealed_at_ts_ms = built_block_data.sealed_at.unix_timestamp_nanos() as i64 / 1_000_000;

                sqlx::query(
                    r#"
                INSERT INTO built_block_data (block_number, orders_closed_at_ts_ms, sealed_at_ts_ms, profit)
                VALUES (?, ?, ?, ?)
                "#,
                ).bind(block_data.block_number as i64)
                    .bind(orders_closed_at_ts_ms)
                    .bind(sealed_at_ts_ms)
                    .bind(format_ether(built_block_data.profit))
                    .execute(conn.as_mut())
                    .await?;
            }


            Ok::<_, eyre::Error>(())
        })).await?;

        Ok(())
    }

    pub async fn read_block_data(&mut self, block_number: u64) -> eyre::Result<BlockData> {
        let block_data = sqlx::query(
            r#"
        SELECT block_number, winning_bid_trace, onchain_block FROM blocks_data
        WHERE block_number = ?
        "#,
        )
        .bind(block_number as i64)
        .fetch_optional(&mut self.conn)
        .await?;
        let block_data = match block_data {
            Some(block_data) => block_data,
            None => return Err(eyre::eyre!("Block data not found")),
        };

        let orders = sqlx::query(
            r#"
        SELECT block_number, timestamp_ms, order_data FROM orders
        WHERE block_number = ?
        "#,
        )
        .bind(block_number as i64)
        .fetch_all(&mut self.conn)
        .await?;

        let built_block_data = sqlx::query(
            r#"
        SELECT block_number, orders_closed_at_ts_ms, sealed_at_ts_ms, profit FROM built_block_data
        WHERE block_number = ?
        "#,
        )
        .bind(block_number as i64)
        .fetch_all(&mut self.conn)
        .await?;

        let built_block_included_orders = sqlx::query(
            r#"
        SELECT block_number, order_id FROM built_block_included_orders
        WHERE block_number = ?
        "#,
        )
        .bind(block_number as i64)
        .fetch_all(&mut self.conn)
        .await?;

        group_rows_into_block_data(
            vec![block_data],
            orders,
            built_block_data,
            built_block_included_orders,
        )
        .map(|mut v| v.remove(0))
    }

    /// Retunrs BlockData for the given block, if some blocks are missing error is not returned.
    /// WARN: will load into memory everything for blocks in range: min(blocks), max(blocks)
    pub async fn read_blocks(&mut self, blocks: &[u64]) -> eyre::Result<Vec<BlockData>> {
        let min_block = blocks.iter().min().copied().unwrap_or_default() as i64;
        let max_block = blocks.iter().max().copied().unwrap_or_default() as i64;

        let block_data = sqlx::query(
            r#"
        SELECT block_number, winning_bid_trace, onchain_block FROM blocks_data
        WHERE block_number between ? and ?
        "#,
        )
        .bind(min_block)
        .bind(max_block)
        .fetch_all(&mut self.conn)
        .await?;

        let orders = sqlx::query(
            r#"
        SELECT block_number, timestamp_ms, order_data FROM orders
        WHERE block_number between ? and ?
        "#,
        )
        .bind(min_block)
        .bind(max_block)
        .fetch_all(&mut self.conn)
        .await?;

        let built_block_data = sqlx::query(
            r#"
        SELECT block_number, orders_closed_at_ts_ms, sealed_at_ts_ms, profit FROM built_block_data
        WHERE block_number between ? and ?
        "#,
        )
        .bind(min_block)
        .bind(max_block)
        .fetch_all(&mut self.conn)
        .await?;

        let built_block_included_orders = sqlx::query(
            r#"
        SELECT block_number, order_id FROM built_block_included_orders
        WHERE block_number between ? and ?
        "#,
        )
        .bind(min_block)
        .bind(max_block)
        .fetch_all(&mut self.conn)
        .await?;

        let mut res = group_rows_into_block_data(
            block_data,
            orders,
            built_block_data,
            built_block_included_orders,
        )?;
        let blocks = blocks.iter().collect::<HashSet<_>>();
        res.retain(|block| blocks.contains(&block.block_number));
        Ok(res)
    }

    pub async fn get_blocks(&mut self) -> eyre::Result<Vec<u64>> {
        let blocks = sqlx::query_as::<_, (i64,)>(
            r#"
            SELECT block_number FROM blocks
            ORDER BY block_number ASC
        "#,
        )
        .fetch_all(&mut self.conn)
        .await?;

        Ok(blocks
            .into_iter()
            .map(|(block_number,)| block_number.try_into())
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub async fn get_blocks_info(&mut self) -> eyre::Result<Vec<HistoricalBlocksInfo>> {
        let blocks = sqlx::query_as::<_, (i64, String, String, String, i64)>(
            r#"
            SELECT blocks.block_number, block_hash, fee_recipient, bid_value, order_count FROM blocks
            JOIN (
                SELECT block_number, count(*) as order_count FROM orders GROUP BY block_number
            ) as order_count ON blocks.block_number = order_count.block_number
            ORDER BY blocks.block_number ASC
        "#,
        )
            .fetch_all(&mut self.conn)
            .await?;

        blocks
            .into_iter()
            .map(
                |(block_number, block_hash, fee_recipient, bid_value, order_count)| -> eyre::Result<HistoricalBlocksInfo> {
                    Ok(HistoricalBlocksInfo {
                        block_number: block_number.try_into()?,
                        block_hash: block_hash.parse()?,
                        fee_recipient: fee_recipient.parse()?,
                        bid_value: parse_ether(&bid_value)?,
                        order_count: order_count.try_into()?,
                    })
                }
            )
            .collect::<Result<Vec<_>, _>>()
    }

    pub async fn remove_block(&mut self, block_number: u64) -> eyre::Result<()> {
        self.conn
            .transaction(|conn| {
                Box::pin(async move {
                    sqlx::query(
                        r#"
            DELETE FROM blocks WHERE block_number = ?;
            "#,
                    )
                    .bind(block_number as i64)
                    .execute(conn.as_mut())
                    .await?;
                    sqlx::query(
                        r#"
            DELETE FROM orders WHERE block_number = ?;
            "#,
                    )
                    .bind(block_number as i64)
                    .execute(conn.as_mut())
                    .await?;
                    sqlx::query(
                        r#"
            DELETE FROM blocks_data WHERE block_number = ?;
            "#,
                    )
                    .bind(block_number as i64)
                    .execute(conn.as_mut())
                    .await?;
                    Ok::<_, eyre::Error>(())
                })
            })
            .await?;

        Ok(())
    }
}

fn order_type(order: &RawOrder) -> &'static str {
    match order {
        RawOrder::Bundle(_) => "bundle",
        RawOrder::Tx(_) => "tx",
        RawOrder::ShareBundle(_) => "sbundle",
    }
}

fn compress_data(data: &[u8]) -> Vec<u8> {
    compress_prepend_size(data)
}

fn decompress_data(data: &[u8]) -> Result<Vec<u8>, DecompressError> {
    decompress_size_prepended(data)
}

fn group_rows_into_block_data(
    blocks_data: Vec<SqliteRow>,
    orders: Vec<SqliteRow>,
    built_block_data: Vec<SqliteRow>,
    built_block_included_orders: Vec<SqliteRow>,
) -> eyre::Result<Vec<BlockData>> {
    let mut block_data_by_block = blocks_data
        .into_par_iter()
        .map(|block_data| -> eyre::Result<(u64, BlockData)> {
            let block_number = block_data.try_get::<i64, _>("block_number")? as u64;
            let winning_bid_trace: BuilderBlockReceived = serde_json::from_slice(
                &decompress_data(&block_data.try_get::<Vec<u8>, _>("winning_bid_trace")?)?,
            )?;
            let onchain_block: alloy_rpc_types::Block = serde_json::from_slice(&decompress_data(
                &block_data.try_get::<Vec<u8>, _>("onchain_block")?,
            )?)?;
            Ok((
                block_number,
                BlockData {
                    block_number,
                    winning_bid_trace,
                    onchain_block,
                    available_orders: Vec::new(),
                    built_block_data: None,
                },
            ))
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .collect::<HashMap<_, _>>();

    let mut built_blocks_data = built_block_data
        .into_par_iter()
        .map(|built_block_data| -> eyre::Result<(u64, BuiltBlockData)> {
            let block_number = built_block_data.try_get::<i64, _>("block_number")? as u64;
            let orders_closed_at_ts_ms =
                built_block_data.try_get::<i64, _>("orders_closed_at_ts_ms")?;
            let sealed_at_ts_ms = built_block_data.try_get::<i64, _>("sealed_at_ts_ms")?;
            let profit = built_block_data.try_get::<String, _>("profit")?;
            let profit = match ParseUnits::parse_units(&profit, Unit::ETHER)? {
                ParseUnits::U256(u) => I256::try_from(u)?,
                ParseUnits::I256(i) => i,
            };

            Ok((
                block_number,
                BuiltBlockData {
                    included_orders: vec![],
                    orders_closed_at: timestamp_ms_to_offset_datetime(
                        orders_closed_at_ts_ms as u64,
                    ),
                    sealed_at: timestamp_ms_to_offset_datetime(sealed_at_ts_ms as u64),
                    profit,
                },
            ))
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .collect::<HashMap<_, _>>();

    for row in built_block_included_orders {
        let block_number = row.try_get::<i64, _>("block_number")? as u64;
        let order_id = row.try_get::<String, _>("order_id")?;
        let order_id = OrderId::from_str(&order_id)?;

        built_blocks_data
            .get_mut(&block_number)
            .ok_or_else(|| eyre::eyre!("Block data {} not found", block_number))?
            .included_orders
            .push(order_id);
    }
    for (block_number, built_block_data) in built_blocks_data {
        if let Some(block_data) = block_data_by_block.get_mut(&block_number) {
            block_data.built_block_data = Some(built_block_data);
        }
    }

    let orders_by_blocks = orders
        .into_par_iter()
        .map(|row| -> eyre::Result<(u64, OrdersWithTimestamp)> {
            let order_data = decompress_data(&row.try_get::<Vec<u8>, _>("order_data")?)?;
            let block_number = row.try_get::<i64, _>("block_number")? as u64;
            let raw_order: RawOrdersWithTimestamp = serde_json::from_slice(&order_data)?;
            let order: OrdersWithTimestamp = raw_order.decode(TxEncoding::NoBlobData)?;
            Ok((block_number, order))
        })
        .collect::<Result<Vec<_>, _>>()?;

    for (block, order) in orders_by_blocks {
        block_data_by_block
            .get_mut(&block)
            .ok_or_else(|| eyre::eyre!("Block {} not found", block))?
            .available_orders
            .push(order);
    }

    let mut res = block_data_by_block.into_values().collect::<Vec<_>>();
    res.sort_by_key(|v| v.block_number);
    Ok(res)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        backtest::RawOrdersWithTimestamp,
        mev_boost::BuilderBlockReceived,
        primitives::{
            serialize::{RawBundle, RawTx},
            SimValue,
        },
    };
    use alloy_primitives::{hex, Address, Bloom, Bytes, B256};
    use alloy_rpc_types::{Block, BlockTransactions, Header, Signature, Transaction};
    use reth_primitives::{U256, U64};
    use time::OffsetDateTime;
    #[tokio::test]
    async fn test_create_tables() {
        let mut storage = HistoricalDataStorage::new_from_memory().await.unwrap();
        storage.create_tables().await.unwrap();
    }

    #[tokio::test]
    async fn test_write_block_data() {
        let tx = hex!("02f9037b018203cd8405f5e1008503692da370830388ba943fc91a3afd70395cd496c647d5a6cc9d4b2b7fad8780e531581b77c4b903043593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000064f390d300000000000000000000000000000000000000000000000000000000000000030b090c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000009184e72a0000000000000000000000000000000000000000000000000000080e531581b77c400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000b5ea574dd8f2b735424dfc8c4e16760fc44a931b000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000c001a0a9ea84ad107d335afd5e5d2ddcc576f183be37386a9ac6c9d4469d0329c22e87a06a51ea5a0809f43bf72d0156f1db956da3a9f3da24b590b7eed01128ff84a2c1").to_vec();

        let orders: Vec<OrdersWithTimestamp> = vec![
            RawOrdersWithTimestamp {
                timestamp_ms: 10,
                order: RawOrder::Tx(RawTx {
                    tx: tx.clone().into(),
                }),
                sim_value: None,
            }
            .decode(TxEncoding::WithBlobData)
            .unwrap(),
            RawOrdersWithTimestamp {
                timestamp_ms: 11,
                order: RawOrder::Bundle(RawBundle {
                    block_number: U64::from(12),
                    txs: vec![tx.clone().into()],
                    reverting_tx_hashes: vec![],
                    replacement_uuid: Some(uuid::Uuid::from_u128(11)),
                    signing_address: Some(alloy_primitives::address!(
                        "0101010101010101010101010101010101010101"
                    )),
                    min_timestamp: None,
                    max_timestamp: Some(100),
                    replacement_nonce: Some(0),
                }),
                sim_value: Some(SimValue {
                    coinbase_profit: U256::from(42u64),
                    gas_used: 21000,
                    mev_gas_price: U256::from(44u64),
                    ..Default::default()
                }),
            }
            .decode(TxEncoding::WithBlobData)
            .unwrap(),
        ];

        let winning_bid_trace = BuilderBlockReceived {
            slot: 12,
            parent_hash: Default::default(),
            block_hash: Default::default(),
            builder_pubkey: Default::default(),
            proposer_pubkey: Default::default(),
            proposer_fee_recipient: Default::default(),
            gas_limit: 30_000_000,
            gas_used: 15_000_000,
            value: Default::default(),
            num_tx: 2,
            block_number: 12,
            timestamp: 12,
            timestamp_ms: 12000,
            optimistic_submission: false,
        };
        let onchain_block = create_test_block();
        let built_block_data = BuiltBlockData {
            included_orders: vec![OrderId::ShareBundle(B256::random())],
            orders_closed_at: OffsetDateTime::from_unix_timestamp_nanos(1719845355111000000)
                .unwrap(),
            sealed_at: OffsetDateTime::from_unix_timestamp_nanos(1719845355123000000).unwrap(),
            profit: I256::try_from(42).unwrap(),
        };
        let block_data = BlockData {
            block_number: 12,
            winning_bid_trace,
            onchain_block,
            available_orders: orders,
            built_block_data: Some(built_block_data),
        };

        let mut storage = HistoricalDataStorage::new_from_memory().await.unwrap();
        storage.create_tables().await.unwrap();

        storage
            .write_block_data(block_data.clone())
            .await
            .expect("Failed to write block data");
        storage
            .write_block_data(block_data.clone())
            .await
            .expect("Failed to overwrite block data");

        let read_block_data = storage.read_block_data(12).await.unwrap();
        assert_eq!(read_block_data, block_data);

        let blocks = storage.get_blocks().await.unwrap();
        assert_eq!(blocks, vec![12]);
    }

    fn create_empty_block_header() -> Header {
        Header {
            hash: B256::default(),
            parent_hash: B256::default(),
            uncles_hash: B256::default(),
            miner: Address::default(),
            state_root: B256::default(),
            transactions_root: B256::default(),
            receipts_root: B256::default(),
            logs_bloom: Bloom::default(),
            difficulty: U256::default(),
            number: 0,
            gas_limit: 0,
            gas_used: 0,
            timestamp: 0,
            extra_data: Bytes::default(),
            mix_hash: None,
            nonce: None,
            base_fee_per_gas: None,
            withdrawals_root: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
            total_difficulty: None,
            requests_root: None,
        }
    }

    fn create_test_block() -> Block {
        Block {
            header: create_empty_block_header(),
            uncles: Vec::new(),
            // IMPORTANT: Due to what seems to be a bug on BlockTransactions serde serialization we must put a tx
            // since BlockTransactions::Full(empty) deserializes wrongly to BlockTransactions::Hashes(empty)
            transactions: BlockTransactions::Full(vec![create_test_tx()]),
            size: None,
            withdrawals: None,
        }
    }

    fn create_test_tx() -> Transaction {
        Transaction {
            hash: B256::with_last_byte(1),
            nonce: 2,
            block_hash: Some(B256::with_last_byte(3)),
            block_number: Some(4),
            transaction_index: Some(5),
            from: Address::with_last_byte(6),
            to: Some(Address::with_last_byte(7)),
            value: U256::from(8),
            gas_price: Some(9),
            gas: 10,
            input: Bytes::from(vec![11, 12, 13]),
            signature: Some(Signature {
                v: U256::from(14),
                r: U256::from(14),
                s: U256::from(14),
                y_parity: None,
            }),
            chain_id: Some(17),
            blob_versioned_hashes: None,
            access_list: None,
            transaction_type: Some(20),
            max_fee_per_gas: Some(21),
            max_priority_fee_per_gas: Some(22),
            max_fee_per_blob_gas: None,
            authorization_list: None,
        }
    }
}
