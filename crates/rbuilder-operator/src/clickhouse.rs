//! Clickhouse integration to save all the blocks we build and submit to relays.

use alloy_primitives::U256;
use clickhouse::Row;
use rbuilder_utils::clickhouse::{
    backup::primitives::{ClickhouseIndexableData, ClickhouseRowExt},
    serde::{option_u256, vec_u256},
};
use serde::{Deserialize, Serialize};

/// BlockRow to insert in clickhouse and also as entry type for the indexer since the BlockRow is made from a few &objects so it makes no sense to have a Block type and copy all the fields.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct BlockRow {
    pub block_number: u64,
    pub profit: String,
    pub slot: u64,
    pub hash: String,
    pub gas_limit: u64,
    pub gas_used: u64,
    pub base_fee: u64,
    pub parent_hash: String,
    pub proposer_pubkey: String,
    pub proposer_fee_recipient: String,
    pub builder_pubkey: String,
    pub timestamp: u64,
    pub timestamp_datetime: i64,
    pub orders_closed_at: i64,
    pub sealed_at: i64,
    pub algorithm: String,

    #[serde(with = "option_u256")]
    pub true_value: Option<U256>,
    #[serde(with = "option_u256")]
    pub best_relay_value: Option<U256>,
    #[serde(with = "option_u256")]
    pub block_value: Option<U256>,

    pub used_bundle_hashes: Vec<String>,
    pub used_bundle_uuids: Vec<String>,
    pub used_sbundles_hashes: Vec<String>,
    pub delayed_payment_sources: Vec<String>,

    #[serde(with = "vec_u256")]
    pub delayed_payment_values: Vec<U256>,

    pub delayed_payment_addresses: Vec<String>,
}

impl ClickhouseRowExt for BlockRow {
    type TraceId = String;
    const TABLE_NAME: &'static str = "blocks";

    fn trace_id(&self) -> String {
        self.hash.clone()
    }

    fn to_row_ref(row: &Self) -> &<Self as Row>::Value<'_> {
        row
    }
}

impl ClickhouseIndexableData for BlockRow {
    type ClickhouseRowType = BlockRow;

    const DATA_NAME: &'static str = <BlockRow as ClickhouseRowExt>::TABLE_NAME;

    fn trace_id(&self) -> String {
        self.hash.clone()
    }

    fn to_row(self, _builder_name: String) -> Self::ClickhouseRowType {
        self
    }
}
