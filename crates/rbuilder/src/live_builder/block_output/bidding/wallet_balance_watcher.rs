use std::time::Duration;

use alloy_primitives::{Address, BlockNumber, U256};
use reth::{
    primitives::format_ether,
    providers::{HeaderProvider, ProviderError},
};
use reth_provider::StateProviderFactory;
use time::{error, OffsetDateTime};
use tracing::{error, info};

use crate::telemetry::{add_subsidy_value, inc_subsidized_blocks};

use super::interfaces::LandedBlockInfo;

/// Allows to monitor the evolution of our wallet for the landed blocks.
/// It's useful for bidders to detect profit and subsidies.
/// Ugly patch: it also updates metrics.
/// Usage:
/// 1 - Create one and you'll get also the latest history of balance changes.
/// 2 - After each new landed block is detected (or whenever you want) call update_to_block to get info up to that block.
#[derive(Debug)]
pub struct WalletBalanceWatcher<P> {
    provider: P,
    builder_addr: Address,
    /// Last block analyzed. balance is updated up to block_number (included).
    block_number: BlockNumber,
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

impl BlockInfo {
    fn as_landed_block_info(&self, buider_address: &Address) -> LandedBlockInfo {
        LandedBlockInfo {
            block_number: self.block_number,
            block_timestamp: self.timestamp,
            builder_balance: self.builder_balance,
            beneficiary_is_builder: self.beneficiary == *buider_address,
        }
    }
}

impl<P> WalletBalanceWatcher<P>
where
    P: StateProviderFactory + HeaderProvider,
{
    /// Creates a WalletBalanceWatcher pre-analyzing a window of init_window_size size.
    pub fn new(
        provider: P,
        builder_addr: Address,
        init_window_size: Duration,
    ) -> Result<(Self, Vec<LandedBlockInfo>), WalletError> {
        Self {
            provider,
            builder_addr,
            block_number: 0,
            balance: U256::ZERO,
        }
        .init(init_window_size)
    }

    /// Analyzes the past up to subsidy_window_size adding the subsidies.
    /// Returns at least 1 LandedBlockInfo.
    fn init(
        mut self,
        init_window_size: Duration,
    ) -> Result<(Self, Vec<LandedBlockInfo>), WalletError> {
        let analysis_window_limit = OffsetDateTime::now_utc() - init_window_size;
        self.block_number = self.provider.last_block_number()?;
        let mut block_number = self.block_number;
        let last_block_info = self
            .get_block_info(block_number)
            .map_err(|_| WalletError::InitialBalance)?;
        self.balance = last_block_info.builder_balance;

        // Store old balances as balance,ts decreasing ts order
        let mut history = Vec::new();
        while let Ok(block_info) = self.get_block_info(block_number) {
            if block_info.timestamp < analysis_window_limit && !history.is_empty() {
                break;
            }
            history.push(block_info);
            if block_number == 0 {
                break;
            }
            block_number -= 1;
        }
        let landed_block_info_interval = history
            .iter()
            .rev()
            .map(|block_info| block_info.as_landed_block_info(&self.builder_addr))
            .collect();
        Ok((self, landed_block_info_interval))
    }

    /// returns the wallet balance for the block and the block's timestamp
    fn get_block_info(&mut self, block: BlockNumber) -> Result<BlockInfo, WalletError> {
        let builder_balance = self
            .provider
            .history_by_block_number(block)?
            .account_balance(self.builder_addr)?
            .unwrap_or_default();
        let header = self
            .provider
            .header_by_number(block)?
            .ok_or(WalletError::HeaderNotFound(block))?;
        Ok(BlockInfo {
            timestamp: OffsetDateTime::from_unix_timestamp(header.timestamp as i64)?,
            builder_balance,
            block_number: block,
            beneficiary: header.beneficiary,
        })
    }

    /// If a subsidy is detected on the new block it answers its value.
    /// We consider a subsidy as a decrease on balance on a block landed by the builder
    fn get_subsidy(&self, prev_balance: &U256, block_info: &LandedBlockInfo) -> Option<U256> {
        if block_info.builder_balance < *prev_balance && block_info.beneficiary_is_builder {
            Some(prev_balance - block_info.builder_balance)
        } else {
            None
        }
    }

    /// Queries all the new blocks (from last successful update_to_block call or creation).
    pub fn update_to_block(
        &mut self,
        new_block: BlockNumber,
    ) -> Result<Vec<LandedBlockInfo>, WalletError> {
        if new_block <= self.block_number {
            if new_block < self.block_number {
                error!(
                    new_block,
                    self.block_number, "Tried to update WalletBalanceWatcher to the past"
                );
            }
            return Ok(Vec::new());
        }
        let mut res = Vec::new();
        for block_number in self.block_number + 1..=new_block {
            let block_info = self.get_block_info(block_number)?;
            res.push(block_info.as_landed_block_info(&self.builder_addr));
        }
        for landed_block_info in &res {
            // Patch to add subsidy metrics.
            if let Some(subsidy_value) = self.get_subsidy(&self.balance, landed_block_info) {
                info!(
                    block_number = landed_block_info.block_number,
                    subsidy_value = format_ether(subsidy_value),
                    "Subsidy detected"
                );
                add_subsidy_value(subsidy_value, true);
                inc_subsidized_blocks(true);
            }
            self.balance = landed_block_info.builder_balance;
            self.block_number = landed_block_info.block_number;
        }
        Ok(res)
    }
}
