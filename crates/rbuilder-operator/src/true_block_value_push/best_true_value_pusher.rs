//! This module is responsible for syncing the best true value bid between the local state and redis.

use alloy_primitives::{I256, U256};

use parking_lot::Mutex;
use rbuilder::utils::{
    i256decimal_serde_helper,
    reconnect::{run_loop_with_reconnect, RunCommand},
    u256decimal_serde_helper,
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, thread::sleep, time::Duration};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{error, trace};

use crate::metrics::inc_publish_tbv_errors;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BuiltBlockInfo {
    pub timestamp_ms: u64,
    pub block_number: u64,
    pub slot_number: u64,
    /// Best true value of submitted block (has subtracted the payout tx cost)
    #[serde(with = "u256decimal_serde_helper")]
    pub best_true_value: U256,
    /// Bid we made to the relay.
    #[serde(with = "u256decimal_serde_helper")]
    pub bid: U256,
    #[serde(with = "i256decimal_serde_helper")]
    pub subsidy: I256,

    pub builder: String,
    pub slot_end_timestamp: u64,
}

impl BuiltBlockInfo {
    pub fn new(
        block_number: u64,
        slot_number: u64,
        best_true_value: U256,
        bid: U256,
        subsidy: I256,
        builder: String,
        slot_end_timestamp: u64,
    ) -> Self {
        BuiltBlockInfo {
            timestamp_ms: (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as u64,
            block_number,
            slot_number,
            best_true_value,
            bid,
            subsidy,
            builder,
            slot_end_timestamp,
        }
    }

    /// Compares things related to bidding: block_number,slot_number,best_true_value and best_relay_value
    pub fn is_same_bid_info(&self, other: &Self) -> bool {
        self.block_number == other.block_number
            && self.slot_number == other.slot_number
            && self.best_true_value == other.best_true_value
            && self.bid == other.bid
    }
}

#[derive(Debug, Default, Clone)]
pub struct LastBuiltBlockInfoCell {
    data: Arc<Mutex<BuiltBlockInfo>>,
}

impl LastBuiltBlockInfoCell {
    pub fn update_value_safe(&self, value: BuiltBlockInfo) {
        let mut best_value = self.data.lock();
        if value.slot_number < best_value.slot_number {
            // don't update value for the past slot
            return;
        }
        *best_value = value;
    }

    pub fn read(&self) -> BuiltBlockInfo {
        self.data.lock().clone()
    }
}

/// BuiltBlockInfoPusher periodically sends last BuiltBlockInfo via a configurable backend.
#[derive(Debug, Clone)]
pub struct BuiltBlockInfoPusher<BackendType> {
    /// Best value we got from our building algorithms.
    last_local_value: LastBuiltBlockInfoCell,
    backend: BackendType,

    cancellation_token: CancellationToken,
}

const PUSH_INTERVAL: Duration = Duration::from_millis(50);
const MAX_IO_ERRORS: usize = 5;

/// Trait to connect and publish new BuiltBlockInfo data (as a &str)
/// For simplification mixes a little the factory role and the publish role.
pub trait Backend {
    type Connection;
    type BackendError: std::error::Error;
    /// Creates a new connection to the sink of tbv info.
    fn connect(&self) -> Result<Self::Connection, Self::BackendError>;
    /// Call with the connection obtained by connect()
    fn publish(
        &self,
        connection: &mut Self::Connection,
        best_true_value: &BuiltBlockInfo,
    ) -> Result<(), Self::BackendError>;
}

impl<BackendType: Backend> BuiltBlockInfoPusher<BackendType> {
    pub fn new(
        last_local_value: LastBuiltBlockInfoCell,
        backend: BackendType,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            last_local_value,
            backend,
            cancellation_token,
        }
    }

    /// Run the task that pushes the last BuiltBlockInfo.
    /// The value is read from last_local_value and pushed to redis.
    pub fn run_push_task(self) {
        run_loop_with_reconnect(
            "push_best_bid",
            || -> Result<BackendType::Connection, BackendType::BackendError> {
                self.backend.connect()
            },
            |mut conn| -> RunCommand {
                let mut io_errors = 0;
                let mut last_pushed_value: Option<BuiltBlockInfo> = None;
                loop {
                    if self.cancellation_token.is_cancelled() {
                        break;
                    }

                    if io_errors > MAX_IO_ERRORS {
                        return RunCommand::Reconnect;
                    }

                    sleep(PUSH_INTERVAL);
                    let last_local_value = self.last_local_value.read();
                    if last_pushed_value
                        .as_ref()
                        .is_none_or(|value| !value.is_same_bid_info(&last_local_value))
                    {
                        last_pushed_value = Some(last_local_value.clone());
                        match self.backend.publish(&mut conn, &last_local_value) {
                            Ok(()) => {
                                trace!(?last_local_value, "Pushed last local value");
                            }
                            Err(err) => {
                                error!(?err, "Failed to publish last true value bid");
                                // inc_publish_tbv_errors is supposed to be called for block_processor errors but I added the metric here so
                                // it logs for al backends.
                                inc_publish_tbv_errors();
                                io_errors += 1;
                            }
                        }
                    }
                }
                RunCommand::Finish
            },
            self.cancellation_token.clone(),
        )
    }
}
