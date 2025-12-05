use alloy_primitives::{Address, U256};
use rbuilder_config::{EnvOrValue, LoggerConfig};
use serde::Deserialize;
use std::{fs, path::Path};

#[derive(PartialEq, Eq, Debug, Deserialize)]
pub struct RebalancerConfig {
    /// Node RPC URL for block subscription and state fetch.
    pub rpc_url: String,
    /// Builder RPC URL for bundle submissions.
    pub builder_url: String,
    /// Max priority fee per to set on the transfer.
    pub transfer_max_priority_fee_per_gas: U256,
    /// Logger configuration.
    #[serde(flatten)]
    pub logger: LoggerConfig,
    /// Source accounts for funding.
    #[serde(default, rename = "account")]
    pub accounts: Vec<RebalancerAccount<EnvOrValue<String>>>,
    /// Collection of rebelancer rules.
    #[serde(default, rename = "rule")]
    pub rules: Vec<RebalancerRule>,
}

impl RebalancerConfig {
    /// Parse toml file.
    pub fn parse_toml_file(path: &Path) -> eyre::Result<Self> {
        let content = fs::read_to_string(path)?;
        Ok(Self::parse_toml(&content)?)
    }

    /// Parse relay configurations from toml string.
    pub fn parse_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }
}

#[derive(PartialEq, Eq, Debug, Deserialize)]
pub struct RebalancerAccount<S> {
    /// Account ID.
    pub id: String,
    /// Account secret.
    pub secret: S,
    /// Minimum balance for source account.
    pub min_balance: U256,
}

impl<S> RebalancerAccount<S> {
    /// Map account secret.
    pub fn map_secret<F, T>(self, map: F) -> RebalancerAccount<T>
    where
        F: FnOnce(S) -> T,
    {
        RebalancerAccount {
            id: self.id,
            secret: map(self.secret),
            min_balance: self.min_balance,
        }
    }
}

#[derive(PartialEq, Eq, Debug, Deserialize)]
pub struct RebalancerRule {
    /// Rule description.
    pub description: String,
    /// The source of funds referenced by id.
    pub source_id: String,
    /// Destination address.
    pub destination: Address,
    /// Rebalancer rule type and details.
    #[serde(flatten)]
    pub ty: RebalancerRuleType,
}

#[derive(PartialEq, Eq, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RebalancerRuleType {
    Fund {
        /// Destination target balance.
        destination_target_balance: U256,
        /// Destination minimum threshold balance after which funding will be triggered.
        destination_min_balance: U256,
    },
    Sweep {
        /// Source target balance.
        source_target_balance: U256,
        /// Source maximum threshold balance after which sweeping will be triggered.
        source_max_balance: U256,
    },
}

impl RebalancerRuleType {
    /// Returns `true` if the rule is of type [`RebalancerRuleType::Fund`].
    pub fn is_fund(&self) -> bool {
        matches!(self, Self::Fund { .. })
    }

    /// Returns `true` if the rule is of type [`RebalancerRuleType::Sweep`].
    pub fn is_sweep(&self) -> bool {
        matches!(self, Self::Sweep { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config() {
        let config = RebalancerConfig::parse_toml(
            r#"
            rpc_url = ""
            builder_url = ""
            transfer_max_priority_fee_per_gas = "123"

            env_filter = "info"
            log_color = true

            [[rule]]
            description = ""
            source_id = ""
            destination = "0x0000000000000000000000000000000000000000"
            type = "fund"
            destination_target_balance = "1"
            destination_min_balance = "1"

            [[rule]]
            description = ""
            source_id = ""
            destination = "0x0000000000000000000000000000000000000000"
            type = "sweep"
            source_target_balance = "1"
            source_max_balance = "1"
        "#,
        )
        .unwrap();
        assert_eq!(
            config,
            RebalancerConfig {
                rpc_url: String::new(),
                builder_url: String::new(),
                transfer_max_priority_fee_per_gas: U256::from(123),
                logger: LoggerConfig::dev(),
                accounts: Vec::new(),
                rules: Vec::from([
                    RebalancerRule {
                        description: String::new(),
                        source_id: String::new(),
                        destination: Address::ZERO,
                        ty: RebalancerRuleType::Fund {
                            destination_target_balance: U256::from(1),
                            destination_min_balance: U256::from(1),
                        }
                    },
                    RebalancerRule {
                        description: String::new(),
                        source_id: String::new(),
                        destination: Address::ZERO,
                        ty: RebalancerRuleType::Sweep {
                            source_target_balance: U256::from(1),
                            source_max_balance: U256::from(1),
                        }
                    }
                ])
            }
        );
    }
}
