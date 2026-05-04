//! a2r prefix = alloy to reth conversion

use std::time::{Duration, Instant};

use alloy_consensus::TxEnvelope;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Sign, I256, U256};
use alloy_provider::RootProvider;
use rbuilder_primitives::{
    serialize::{RawTx, TxEncoding},
    TransactionSignedEcRecoveredWithBlobs,
};
use reth_chainspec::ChainSpec;
use reth_evm_ethereum::revm_spec_by_timestamp_and_block_number;
use revm::context::CfgEnv;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub mod bls;
pub mod build_info;
pub mod constants;

mod noncer;
pub mod sync;
pub use noncer::NonceCache;

pub mod error_storage;
pub mod fmt;

mod provider_factory_reopen;
pub use provider_factory_reopen::{
    check_block_hash_reader_health, is_provider_factory_health_error, HistoricalBlockError,
    ProviderFactoryReopener, RootHasherImpl,
};

pub mod reconnect;

mod tx_signer;
pub use tx_signer::Signer;

pub mod mevblocker;
pub mod provider_head_state;
pub mod receipts;

#[cfg(test)]
pub mod test_utils;

/// de/serializes U256 as decimal value (U256 serde default is hexa). Needed to interact with some JSONs (eg:ProposerPayloadDelivered in relay provider API)
pub mod u256decimal_serde_helper {
    use std::str::FromStr;

    use alloy_primitives::U256;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &U256, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        //fmt::Display for U256 uses decimal
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<U256, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        //from_str is robust, can take decimal or other prefixed (eg:"0x" hexa) formats.
        U256::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// de/serializes U256 as decimal value (U256 serde default is hexa). Needed to interact with some JSONs (eg:ProposerPayloadDelivered in relay provider API)
pub mod i256decimal_serde_helper {
    use std::str::FromStr;

    use alloy_primitives::I256;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &I256, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        //fmt::Display for I256 uses decimal
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<I256, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        //from_str is robust, can take decimal or other prefixed (eg:"0x" hexa) formats.
        I256::from_str(&s).map_err(serde::de::Error::custom)
    }
}

pub fn http_provider(url: reqwest::Url) -> RootProvider {
    RootProvider::new_http(url)
}

#[cfg(test)]
pub fn set_test_debug_tracing_subscriber() {
    let env = match tracing_subscriber::EnvFilter::try_from_default_env() {
        Ok(env) => env,
        Err(_) => tracing_subscriber::EnvFilter::try_new("rbuilder=trace").unwrap(),
    };
    tracing_subscriber::fmt()
        .with_env_filter(env)
        .with_test_writer()
        .try_init()
        .unwrap_or_default();
}

pub fn get_percent(value: U256, percent: usize) -> U256 {
    (value * U256::from(percent)) / U256::from(100)
}

pub fn a2r_withdrawal(w: alloy_rpc_types::Withdrawal) -> alloy_eips::eip4895::Withdrawal {
    alloy_eips::eip4895::Withdrawal {
        index: w.index,
        validator_index: w.validator_index,
        address: w.address,
        amount: w.amount,
    }
}

/// Panics if it doesn't fit u64 (backwards compatible with previous version).
pub fn timestamp_as_u64(block: &alloy_rpc_types::Block) -> u64 {
    block.header.timestamp
}

/// Returns unix timestamp in milliseconds
pub fn timestamp_now_ms() -> u64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .try_into()
        .unwrap_or_default()
}

pub fn gen_uid() -> u64 {
    rand::random()
}

pub fn default_cfg_env(chain_spec: &ChainSpec, block_timestamp: u64, block_number: u64) -> CfgEnv {
    let spec = revm_spec_by_timestamp_and_block_number(chain_spec, block_timestamp, block_number);
    CfgEnv::new()
        .with_chain_id(chain_spec.chain().id())
        .with_spec(spec)
}

pub fn unix_timestamp_now() -> u64 {
    time::OffsetDateTime::now_utc()
        .unix_timestamp()
        .try_into()
        .unwrap_or_default()
}

pub fn int_percentage(value: u64, percentage: usize) -> u64 {
    value * percentage as u64 / 100
}

/// Cleans block extradata and returns readable representation of it
pub fn clean_extradata(data: &[u8]) -> String {
    String::from_utf8_lossy(data)
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Needed since HashSet does not implement FromIterator
pub fn as_hash_set<T: Eq + std::hash::Hash + Copy>(slice: &[T]) -> ahash::HashSet<T> {
    let mut set = ahash::HashSet::default();
    for t in slice {
        set.insert(*t);
    }
    set
}

/// using u64 for ms is safe since 2^64 ms = 2^64/1000/60/60/24/365 years = 584942417 years.
pub fn offset_datetime_to_timestamp_ms(date: OffsetDateTime) -> u64 {
    (date.unix_timestamp_nanos() / 1_000_000) as u64
}

pub fn timestamp_ms_to_offset_datetime(timestamp: u64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos((timestamp as i128) * 1_000_000)
        .expect("failed to convert timestamp")
}

/// using u64 for us is safe since 2^64 us = 2^64/1000/60/60/24/365 years = 584942 years.
pub fn offset_datetime_to_timestamp_us(date: OffsetDateTime) -> u64 {
    (date.unix_timestamp_nanos() / 1_000) as u64
}

pub fn timestamp_us_to_offset_datetime(timestamp: u64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos((timestamp as i128) * 1_000)
        .expect("failed to convert timestamp")
}

/// returns signer result of a - b
/// panics on overflows
pub fn signed_uint_delta(a: U256, b: U256) -> I256 {
    let a = I256::checked_from_sign_and_abs(Sign::Positive, a).expect("A is too big");
    let b = I256::checked_from_sign_and_abs(Sign::Positive, b).expect("B is too big");
    a.checked_sub(b).expect("Subtraction overflow")
}

pub fn find_suggested_fee_recipient(
    block: &alloy_rpc_types::Block,
    txs: &[TransactionSignedEcRecoveredWithBlobs],
) -> Address {
    let coinbase = block.header.beneficiary;
    let (last_tx_signer, last_tx_to) = if let Some((signer, to)) = txs
        .last()
        .map(|tx| (tx.signer(), tx.to().unwrap_or_default()))
    {
        (signer, to)
    } else {
        return coinbase;
    };

    if last_tx_signer == coinbase {
        last_tx_to
    } else {
        coinbase
    }
}

pub fn extract_onchain_block_txs(
    onchain_block: &alloy_rpc_types::Block,
) -> eyre::Result<Vec<TransactionSignedEcRecoveredWithBlobs>> {
    let mut result = Vec::new();
    for tx in onchain_block.transactions.clone().into_transactions() {
        let tx_envelope: TxEnvelope =
            <alloy_rpc_types_eth::Transaction as Into<TxEnvelope>>::into(tx);
        let encoded = tx_envelope.encoded_2718();
        let tx = RawTx { tx: encoded.into() }.decode(TxEncoding::NoBlobData)?;
        result.push(tx.tx_with_blobs);
    }
    Ok(result)
}

pub fn format_offset_datetime_rfc3339(datetime: &OffsetDateTime) -> String {
    datetime
        .format(&Rfc3339)
        .expect("failed to format datetime")
}

#[inline]
pub fn elapsed_ms(start: Instant) -> f64 {
    duration_ms(start.elapsed())
}

#[inline]
pub fn elapsed_s(start: Instant) -> f64 {
    duration_ms(start.elapsed()) / 1000.0
}

#[inline]
pub fn duration_ms(duration: Duration) -> f64 {
    duration.as_micros() as f64 / 1000.0
}

#[cfg(test)]
mod test {
    use super::*;
    use alloy_eips::eip1559::calculate_block_gas_limit;
    use serde::{Deserialize, Serialize};

    #[test]
    fn test_calc_gas_limit() {
        struct LimitTest {
            parent: u64,
            desired: u64,
            result: u64,
        }
        let tests = vec![
            LimitTest {
                parent: 30_000_000,
                desired: 30_000_000,
                result: 30_000_000,
            },
            LimitTest {
                parent: 30_000_000,
                desired: 29_000_000,
                result: 29_970_705,
            },
            LimitTest {
                parent: 30_000_000,
                desired: 29_999_999,
                result: 29_999_999,
            },
            LimitTest {
                parent: 30_000_000,
                desired: 29_970_705,
                result: 29_970_705,
            },
            LimitTest {
                parent: 30_000_000,
                desired: 31_000_000,
                result: 30_029_295,
            },
            LimitTest {
                parent: 30_000_000,
                desired: 30_029_295,
                result: 30_029_295,
            },
            LimitTest {
                parent: 30_000_000,
                desired: 30_000_001,
                result: 30_000_001,
            },
        ];

        for test in tests {
            let result = calculate_block_gas_limit(test.parent, test.desired);
            assert_eq!(result, test.result);
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct TestStruct {
        #[serde(with = "u256decimal_serde_helper")]
        value: alloy_primitives::U256,
    }
    #[test]
    fn uint_from_decimal_string() {
        let string = r#"{"value":"488045688257417849"}"#;

        let value: TestStruct = serde_json::from_str(string).expect("Failed to parse string");
        assert_eq!(
            value.value,
            alloy_primitives::U256::from(488045688257417849u64)
        );

        let value = serde_json::to_string(&value).expect("Failed to serialize");
        assert_eq!(value, string);
    }
}
