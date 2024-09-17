use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, BlockNumber, U256};
use reth::{
    primitives::format_ether,
    providers::{BlockNumReader, HeaderProvider, ProviderError, ProviderFactory},
};
use reth_db::DatabaseEnv;
use time::{error, OffsetDateTime};
use tracing::{error, info};

use crate::telemetry::{add_subsidy_value, inc_subsidized_blocks};

use super::interfaces::{LandedBlockInfo, LandedBlockIntervalInfo};

/// Allows to monitor the evolution of our wallet for the blocks WE land.
/// It's useful for bidders to detect profit and subsidies.
#[derive(Debug)]
pub struct WalletBalanceWatcher {
    provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
    builder_addr: Address,
    /// Last block analyzed. balance is updated up to block_number (included).
    block_number: BlockNumber,
    block_timestamp: OffsetDateTime,
    balance: U256,
}

#[derive(thiserror::Error, Debug)]
pub enum WalletError {
    #[error("ProviderError: {0}")]
    ProviderError(#[from] ProviderError),
    #[error("HeaderNotFound for block {0}")]
    HeaderNotFound(BlockNumber),
    #[error("Invalid timestamp {0}")]
    InvalidTimestamp(#[from] error::ComponentRange),
    #[error("Unable to get initial balance")]
    InitialBalance,
}

struct BlockInfo {
    timestamp: OffsetDateTime,
    builder_balance: U256,
    beneficiary: Address,
    block_number: BlockNumber,
}

impl WalletBalanceWatcher {
    /// Creates a WalletBalanceWatcher pre-analyzing a window of init_window_size size.
    pub fn new(
        provider_factory: ProviderFactory<Arc<DatabaseEnv>>,
        builder_addr: Address,
        init_window_size: Duration,
    ) -> Result<(Self, LandedBlockIntervalInfo), WalletError> {
        Self {
            provider_factory,
            builder_addr,
            block_number: 0,
            balance: U256::ZERO,
            // ugly default :(
            block_timestamp: OffsetDateTime::now_utc(),
        }
        .init(init_window_size)
    }

    /// LandedBlockInfo is the block is landed by builder_addr.
    fn get_landed_block_info(
        &self,
        prev_balance: &U256,
        block_info: &BlockInfo,
    ) -> Option<LandedBlockInfo> {
        if self.builder_addr == block_info.beneficiary {
            Some(LandedBlockInfo {
                prev_balance: *prev_balance,
                new_balance: block_info.builder_balance,
                block_number: block_info.block_number,
                block_timestamp: block_info.timestamp,
            })
        } else {
            None
        }
    }

    /// Analyzes the past up to subsidy_window_size adding the subsidies
    fn init(
        mut self,
        init_window_size: Duration,
    ) -> Result<(Self, LandedBlockIntervalInfo), WalletError> {
        let analysis_window_limit = OffsetDateTime::now_utc() - init_window_size;
        let last_block_number = self.provider_factory.last_block_number()?;
        let mut block_number = last_block_number;
        let BlockInfo {
            timestamp: last_timestamp,
            builder_balance: _,
            beneficiary: _,
            block_number: _,
        } = self.get_block_info(block_number)?;

        // Store old balances as balance,ts decreasing ts order
        let mut history = Vec::new();
        while let Ok(block_info) = self.get_block_info(block_number) {
            if block_info.timestamp < analysis_window_limit {
                break;
            }
            history.push(block_info);
            if block_number == 0 {
                break;
            }
            block_number -= 1;
        }
        // Review history adding subsidies
        let BlockInfo {
            timestamp: start_timestamp,
            builder_balance: mut history_balance,
            beneficiary: _,
            block_number: _,
        } = history.last().ok_or(WalletError::InitialBalance)?;

        let mut landed_block_interval_info = LandedBlockIntervalInfo {
            landed_blocks: Vec::new(),
            first_analyzed_block_time_stamp: *start_timestamp,
            last_analyzed_block_time_stamp: last_timestamp,
        };
        for block_info in history.iter().rev().skip(1) {
            if let Some(landed_block_info) =
                self.get_landed_block_info(&history_balance, block_info)
            {
                landed_block_interval_info
                    .landed_blocks
                    .push(landed_block_info);
            }
            history_balance = block_info.builder_balance;
        }
        self.balance = history_balance;
        self.block_number = last_block_number;
        self.block_timestamp = last_timestamp;
        Ok((self, landed_block_interval_info))
    }

    /// returns the wallet balance for the block and the block's timestamp
    fn get_block_info(&mut self, block: BlockNumber) -> Result<BlockInfo, WalletError> {
        let builder_balance = self
            .provider_factory
            .history_by_block_number(block)?
            .account_balance(self.builder_addr)?
            .unwrap_or_default();
        let header = self
            .provider_factory
            .header_by_number(block)?
            .ok_or(WalletError::HeaderNotFound(block))?;
        Ok(BlockInfo {
            timestamp: OffsetDateTime::from_unix_timestamp(header.timestamp as i64)?,
            builder_balance,
            block_number: block,
            beneficiary: header.beneficiary,
        })
    }

    /// Queries all the new blocks (from last successful update_to_block call or creation) WE landed.
    pub fn update_to_block(
        &mut self,
        new_block: BlockNumber,
    ) -> Result<LandedBlockIntervalInfo, WalletError> {
        if new_block <= self.block_number {
            if new_block < self.block_number {
                error!(
                    new_block,
                    self.block_number, "Tried to update WalletBalanceWatcher to the past"
                );
            }
            // Mmmmm.. timestamps are wrong since they are inclusive....
            return Ok(LandedBlockIntervalInfo {
                landed_blocks: Vec::new(),
                first_analyzed_block_time_stamp: self.block_timestamp,
                last_analyzed_block_time_stamp: self.block_timestamp,
            });
        }
        let mut res = LandedBlockIntervalInfo {
            landed_blocks: Vec::new(),
            first_analyzed_block_time_stamp: self.block_timestamp,
            last_analyzed_block_time_stamp: self.block_timestamp,
        };
        let mut balance = self.balance;
        for block_number in self.block_number + 1..=new_block {
            let block_info = self.get_block_info(block_number)?;
            if let Some(landed_block_info) = self.get_landed_block_info(&balance, &block_info) {
                // We should remove metrics from here!
                if landed_block_info.new_balance < landed_block_info.prev_balance {
                    // Update subsidy metrics.
                    let subsidy_value =
                        landed_block_info.prev_balance - landed_block_info.new_balance;
                    info!(
                        block_number,
                        subsidy_value = format_ether(subsidy_value),
                        "Subsidy detected"
                    );
                    add_subsidy_value(subsidy_value, true);
                    inc_subsidized_blocks(true);
                }
                res.landed_blocks.push(landed_block_info);
            }
            balance = block_info.builder_balance;

            if block_number == self.block_number {
                res.first_analyzed_block_time_stamp = self.block_timestamp;
            }

            if block_number == new_block {
                // If we are here nothing can fail.
                self.balance = block_info.builder_balance;
                self.block_number = block_number;
                self.block_timestamp = block_info.timestamp;
            }
        }
        res.last_analyzed_block_time_stamp = self.block_timestamp;
        Ok(res)
    }
}
