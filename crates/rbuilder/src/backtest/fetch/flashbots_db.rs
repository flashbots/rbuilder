use crate::{
    backtest::{
        fetch::data_source::{BlockRef, DataSource, DatasourceData},
        BuiltBlockData, OrdersWithTimestamp,
    },
    primitives::{
        serialize::{RawBundle, RawOrder, RawShareBundle, TxEncoding},
        Order, OrderId, SimValue,
    },
};
use alloy_primitives::I256;
use async_trait::async_trait;
use bigdecimal::{
    num_bigint::{BigInt, Sign, ToBigInt},
    BigDecimal,
};
use eyre::WrapErr;
use reth_primitives::{Bytes, B256, U256, U64};
use sqlx::postgres::PgPool;
use std::{collections::HashSet, ops::Mul, str::FromStr};
use time::{OffsetDateTime, PrimitiveDateTime};
use tracing::trace;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct RelayDB {
    pool: PgPool,
}

impl RelayDB {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl RelayDB {
    pub async fn get_simulated_bundles_for_block(
        &self,
        block: u64,
    ) -> Result<Vec<OrdersWithTimestamp>, eyre::Error> {
        let block = i64::try_from(block)?;

        let bundles = sqlx::query_as::<_, (OffsetDateTime, String, i64, String, String, Option<sqlx::types::BigDecimal>, Option<i64>, Option<String>, Option<Uuid>, Option<i64>)>(
            "SELECT \
                 inserted_at, bundle_hash, param_block_number, param_signed_txs, param_reverting_tx_hashes, \
                 coinbase_diff, total_gas_used, signing_address, replacement_uuid, param_timestamp \
                 FROM bundles \
                 WHERE is_simulated = true and param_block_number = $1\
                 ORDER BY inserted_at ASC",
        )
        .bind(block)
        .fetch_all(&self.pool)
        .await?;

        let bundle_result = bundles
            .into_iter()
            .map(
                |(
                    inserted_at,
                    bundle_hash,
                    block,
                    txs,
                    reverting_tx_hashes,
                    coinbase_diff,
                    total_gas_used,
                    signing_address,
                    replacement_uuid,
                    min_timestamp,
                )|
                 -> eyre::Result<OrdersWithTimestamp> {
                    let txs = txs
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(Bytes::from_str)
                        .collect::<Result<Vec<_>, _>>()
                        .wrap_err_with(|| {
                            format!("Failed to parse txs for bundle {}", bundle_hash)
                        })?;
                    let reverting_tx_hashes = reverting_tx_hashes
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(B256::from_str)
                        .collect::<Result<_, _>>()
                        .wrap_err_with(|| {
                            format!(
                                "Failed to parse reverting tx hashes for bundle {}",
                                bundle_hash
                            )
                        })?;

                    if txs.is_empty() {
                        return Err(eyre::eyre!("Bundle {} has no txs", bundle_hash));
                    }

                    let signing_address = if let Some(address) = signing_address {
                        Some(address.parse().wrap_err_with(|| {
                            format!("Failed to parse signing address for bundle {}", bundle_hash)
                        })?)
                    } else {
                        None
                    };

                    let raw_bundle = RawBundle {
                        block_number: U64::from(block),
                        txs,
                        reverting_tx_hashes,
                        replacement_uuid,
                        signing_address,
                        min_timestamp: min_timestamp.map(|ts| ts.try_into().unwrap_or_default()),
                        max_timestamp: None,
                        replacement_nonce: replacement_uuid.and(Some(0)),
                    };

                    let sim_value = coinbase_diff.zip(total_gas_used).and_then(|(cb, gas)| {
                        let coinbase_profit = sql_eth_decimal_to_wei(cb)?;
                        let mev_gas_price = coinbase_profit / U256::try_from(gas).ok()?;
                        Some(SimValue {
                            coinbase_profit,
                            gas_used: gas.try_into().ok()?,
                            mev_gas_price,
                            ..Default::default()
                        })
                    });

                    let order = RawOrder::Bundle(raw_bundle)
                        .decode(TxEncoding::NoBlobData)
                        .wrap_err_with(|| format!("Failed to parse bundle {}", bundle_hash))?;

                    Ok(OrdersWithTimestamp {
                        timestamp_ms: (inserted_at.unix_timestamp_nanos() / 1_000_000)
                            .try_into()?,
                        order,
                        sim_value,
                    })
                },
            )
            .collect::<Vec<_>>();

        let bundles = bundle_result
            .into_iter()
            .filter_map(|res| match res {
                Ok(bundle) => Some(bundle),
                Err(err) => {
                    tracing::warn!(err = ?err, "Failed to parse bundle");
                    None
                }
            })
            .collect();

        Ok(bundles)
    }

    pub async fn get_simulated_share_bundles_for_block(
        &self,
        block: u64,
        block_timestamp: OffsetDateTime,
    ) -> Result<Vec<OrdersWithTimestamp>, eyre::Error> {
        let from_time = block_timestamp - time::Duration::seconds(26 * 12);
        let to_time = block_timestamp;

        let simulated_bundles = sqlx::query_as::<
            _,
            (
                OffsetDateTime,
                Vec<u8>,
                sqlx::types::JsonValue,
                Option<sqlx::types::BigDecimal>,
                Option<i64>,
            ),
        >(
            "SELECT received_at, hash, body, sim_profit, sim_gas_used \
                 FROM sbundle \
                 WHERE sim_success = true and cancelled = false and inserted_at between $1 and $2 \
                 ORDER BY inserted_at ASC",
        )
        .bind(from_time)
        .bind(to_time)
        .fetch_all(&self.pool)
        .await?;

        // We pull sbundles that builder actually used for the given block because db overwrite may
        // change block range and timestamp where bundle can be applied after the fact and we miss them.
        let used_bundles = sqlx::query_as::<
            _,
            (
                PrimitiveDateTime,
                Vec<u8>,
                sqlx::types::JsonValue,
                Option<sqlx::types::BigDecimal>,
                Option<i64>,
            ),
        >(
            "WITH used_sbundles AS (
                SELECT DISTINCT ON (sbu.hash) sbu.hash, bb.orders_closed_at as received_at
                FROM sbundle_builder_used sbu JOIN built_blocks bb ON sbu.block_id = bb.block_id
                WHERE bb.block_number = $1
                ORDER BY sbu.hash, bb.orders_closed_at ASC
            ) SELECT us.received_at, s.hash, s.body, s.sim_profit, s.sim_gas_used
            FROM used_sbundles us JOIN sbundle s ON us.hash = s.hash",
        )
        .bind(block as i64)
        .fetch_all(&self.pool)
        .await?;

        let bundles = simulated_bundles.into_iter().map(|v| (v, false)).chain(
            used_bundles
                .into_iter()
                .map(|v| ((v.0.assume_utc(), v.1, v.2, v.3, v.4), true)),
        );

        let bundles = bundles
            .map(
                |((received_at, hash, body, coinbase_diff, total_gas_used), used_sbundle)|
                 -> eyre::Result<(u64, RawShareBundle, Option<SimValue>, B256)> {
                    let hash = (hash.len() == 32).then(|| B256::from_slice(&hash)).ok_or_else(|| eyre::eyre!("Invalid hash length"))?;
                    let mut bundle = serde_json::from_value::<RawShareBundle>(body)
                        .wrap_err_with(|| {
                            format!("Failed to parse share bundle {:?}", hash)
                        })?;
                    // if it was used by the live builder we are sure that it has correct block range
                    // so we modify it here to correct db overwrites
                    if used_sbundle {
                        bundle.inclusion.block = U64::from(block);
                        bundle.inclusion.max_block = None;
                    }

                    let sim_value = coinbase_diff.zip(total_gas_used).and_then(|(cb, gas)| {
                        let coinbase_profit = sql_eth_decimal_to_wei(cb)?;
                        let mev_gas_price = coinbase_profit / U256::try_from(gas).ok()?;
                        Some(SimValue {
                            coinbase_profit,
                            gas_used: gas.try_into().ok()?,
                            mev_gas_price,
                            ..Default::default()
                        })
                    });

                    Ok((
                        (received_at.unix_timestamp_nanos() / 1_000_000)
                            .try_into()?,
                        bundle,
                        sim_value,
                        hash,
                    ))
                },
            )
            .collect::<Result<Vec<_>, _>>()?;

        let mut result = Vec::with_capacity(bundles.len());
        let mut inserted_bundles: HashSet<B256> = HashSet::default();

        for (timestamp_ms, bundle, sim_value, hash) in bundles {
            if inserted_bundles.contains(&hash) {
                continue;
            }
            let from = bundle.inclusion.block.to::<u64>();
            let to = bundle
                .inclusion
                .max_block
                .unwrap_or(bundle.inclusion.block)
                .to::<u64>();

            if !(from <= block && block <= to) {
                continue;
            }

            let raw_order = RawOrder::ShareBundle(bundle);

            let order: Order = raw_order
                .decode(TxEncoding::NoBlobData)
                .wrap_err_with(|| format!("Failed to parse share bundle: {:?}", hash))?;

            result.push(OrdersWithTimestamp {
                timestamp_ms,
                order,
                sim_value,
            });
            inserted_bundles.insert(hash);
        }

        Ok(result)
    }

    pub async fn get_built_block_data(
        &self,
        block_hash: B256,
    ) -> eyre::Result<Option<BuiltBlockData>> {
        let block_hash = format!("{:?}", block_hash);

        let mut built_blocks = sqlx::query_as::<
            _,
            (
                PrimitiveDateTime,
                PrimitiveDateTime,
                Option<sqlx::types::BigDecimal>,
                Option<sqlx::types::BigDecimal>,
            ),
        >(
            "SELECT orders_closed_at, sealed_at, true_value, block_value from built_blocks bb \
                  WHERE bb.hash = $1 \
                  ORDER BY inserted_at DESC LIMIT 1",
        )
        .bind(&block_hash)
        .fetch_all(&self.pool)
        .await?;

        let (orders_closed_at, sealed_at, true_value, block_value) = match built_blocks.pop() {
            Some(built_block) => built_block,
            None => {
                return Ok(None);
            }
        };

        let included_bundles = sqlx::query_as::<_, (Uuid,)>(
            "select distinct b.bundle_uuid from built_blocks bb
            join built_blocks_bundles bbb on bb.block_id = bbb.block_id
            join bundles b on b.id = bbb.bundle_id
            where bb.hash = $1 and b.bundle_uuid is not null",
        )
        .bind(&block_hash)
        .fetch_all(&self.pool)
        .await?;

        let included_sbundles = sqlx::query_as::<_, (Vec<u8>,)>(
            "SELECT DISTINCT sbu.hash FROM built_blocks bb
            JOIN sbundle_builder_used sbu ON bb.block_id = sbu.block_id
            WHERE bb.hash = $1 AND sbu.inserted = true",
        )
        .bind(&block_hash)
        .fetch_all(&self.pool)
        .await?;

        let mut included_orders = Vec::new();
        for (bundle_uuid,) in included_bundles {
            let order_id = OrderId::Bundle(bundle_uuid);
            included_orders.push(order_id);
        }
        for (sbundle_hash,) in included_sbundles {
            let order_id = OrderId::ShareBundle(B256::from_slice(&sbundle_hash));
            included_orders.push(order_id);
        }

        let true_value = I256::try_from(
            true_value
                .and_then(sql_wei_decimal_to_wei)
                .unwrap_or_default(),
        )?;
        let block_value = I256::try_from(
            block_value
                .and_then(sql_wei_decimal_to_wei)
                .unwrap_or_default(),
        )?;

        Ok(Some(BuiltBlockData {
            included_orders,
            orders_closed_at: orders_closed_at.assume_utc(),
            sealed_at: sealed_at.assume_utc(),
            profit: true_value - block_value,
        }))
    }
}

#[async_trait]
impl DataSource for RelayDB {
    async fn get_data(&self, block: BlockRef) -> eyre::Result<DatasourceData> {
        let bundles = self
            .get_simulated_bundles_for_block(block.block_number)
            .await
            .with_context(|| format!("Failed to fetch bundles for block {}", block.block_number))?;

        let block_timestamp = OffsetDateTime::from_unix_timestamp(block.block_timestamp as i64)?;
        let share_bundles = self
            .get_simulated_share_bundles_for_block(block.block_number, block_timestamp)
            .await
            .with_context(|| {
                format!(
                    "Failed to fetch share bundles for block {}",
                    block.block_number
                )
            })?;

        trace!(
            "Fetched bundles from flashbots db, bundles: {}, sbundles: {}",
            bundles.len(),
            share_bundles.len()
        );

        let built_block_data = if let Some(block_hash) = block.landed_block_hash {
            self.get_built_block_data(block_hash)
                .await
                .with_context(|| {
                    format!("Failed to fetch built block data for block {}", block_hash)
                })?
        } else {
            None
        };

        Ok(DatasourceData {
            orders: bundles
                .into_iter()
                .chain(share_bundles.into_iter())
                .collect(),
            built_block_data,
        })
    }

    fn clone_box(&self) -> Box<dyn DataSource> {
        Box::new(self.clone())
    }
}

fn sql_wei_decimal_to_wei(val: sqlx::types::BigDecimal) -> Option<U256> {
    let (bi, exp) = val.into_bigint_and_exponent();
    let (bi_sign, bi_bytes) = bi.to_bytes_be();
    let bi = BigInt::from_bytes_be(bi_sign, &bi_bytes);
    let cb = BigDecimal::new(bi, exp);

    let cb = cb.to_bigint()?;
    let (sign, bytes) = cb.to_bytes_be();
    if sign == Sign::Minus {
        return None;
    }
    let cb = U256::try_from_be_slice(&bytes)?;

    Some(cb)
}

fn sql_eth_decimal_to_wei(val: sqlx::types::BigDecimal) -> Option<U256> {
    let (bi, exp) = val.into_bigint_and_exponent();
    let (bi_sign, bi_bytes) = bi.to_bytes_be();
    let bi = BigInt::from_bytes_be(bi_sign, &bi_bytes);
    let cb = BigDecimal::new(bi, exp);

    let eth = BigDecimal::new(1u64.into(), -18);
    let cb = cb.mul(eth);
    let cb = cb.to_bigint()?;
    let (sign, bytes) = cb.to_bytes_be();
    if sign == Sign::Minus {
        return None;
    }
    let cb = U256::try_from_be_slice(&bytes)?;

    Some(cb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::set_test_debug_tracing_subscriber;
    use sqlx::postgres::PgPoolOptions;

    #[tokio::test]
    #[ignore]
    async fn test_get_simulated_bundles_for_block() {
        set_test_debug_tracing_subscriber();

        let conn_str = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&conn_str)
            .await
            .unwrap();

        let db = RelayDB::new(pool);

        let bundles = db
            .get_simulated_bundles_for_block(18064240)
            .await
            .expect("Failed to get bundles");

        assert_eq!(bundles.len(), 150);

        tracing::trace!("bundles: {:#?}", bundles[bundles.len() - 1]);
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_simulated_share_bundles_for_block() {
        set_test_debug_tracing_subscriber();

        let conn_str = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&conn_str)
            .await
            .unwrap();

        let db = RelayDB::new(pool);

        let block_ts = OffsetDateTime::from_unix_timestamp(1696937351).unwrap();

        let bundles = db
            .get_simulated_share_bundles_for_block(18319762, block_ts)
            .await
            .expect("Failed to get share bundles");

        dbg!(bundles.len());

        assert_eq!(bundles.len(), 117);

        tracing::trace!("bundles: {:#?}", bundles[bundles.len() - 1]);
    }

    #[test]
    fn test_sql_decimal_to_wei() {
        set_test_debug_tracing_subscriber();

        let val = sqlx::types::BigDecimal::from_str("0.000000000000000001").unwrap();
        let wei = sql_eth_decimal_to_wei(val);
        assert_eq!(wei, Some(U256::from(1u64)));

        let val = sqlx::types::BigDecimal::from_str("0.100000000000000001").unwrap();
        let wei = sql_eth_decimal_to_wei(val);
        assert_eq!(wei, Some(U256::from(100000000000000001u64)));

        let val = sqlx::types::BigDecimal::from_str("-1").unwrap();
        let wei = sql_eth_decimal_to_wei(val);
        assert_eq!(wei, None);

        let val = sqlx::types::BigDecimal::from_str("0.00000000000000000000001").unwrap();
        let wei = sql_eth_decimal_to_wei(val);
        assert_eq!(wei, Some(U256::from(0u64)));
    }
}
