use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use clap::Parser;
use rbuilder_rebalancer::{config::RebalancerConfig, rebalancer::Rebalancer};
use std::{path::PathBuf, str::FromStr, time::Duration};
use tracing::*;

#[tokio::main]
async fn main() {
    if let Err(error) = Cli::parse().run().await {
        eprintln!("Error: {error:?}");
        std::process::exit(1);
    }
}

#[derive(Parser)]
struct Cli {
    #[clap(env = "REBALANCER_CONFIG", help = "Config file path")]
    config: PathBuf,
}

impl Cli {
    async fn run(self) -> eyre::Result<()> {
        let config = RebalancerConfig::parse_toml_file(&self.config)?;

        config.logger.init_tracing()?;

        if config.rules.is_empty() {
            warn!("No rebalancing rules have been configured, rebalancer will be idling");
        }

        for rule in &config.rules {
            if rule.destination_min_balance >= rule.destination_target_balance {
                eyre::bail!("Invalid configuration for rule `{}`: minimum balance must be lower than the target", rule.description);
            }

            if !config.accounts.iter().any(|acc| acc.id == rule.source_id) {
                eyre::bail!("Invalid configuration for rule `{}`: account entry is missing for source account {}", rule.description, rule.source_id);
            }
        }

        let rpc_provider = ProviderBuilder::new().connect(&config.rpc_url).await?;
        let transfer_max_priority_fee_per_gas =
            config.transfer_max_priority_fee_per_gas.try_into().unwrap();
        let accounts = config
            .accounts
            .into_iter()
            .map(|account| {
                let account = account.map_secret(|secret| {
                    let secret = secret.value().expect("invalid env");
                    PrivateKeySigner::from_str(&secret).expect("invalid private key")
                });
                (account.id.clone(), account)
            })
            .collect();
        Rebalancer::new(
            rpc_provider,
            config.builder_url,
            transfer_max_priority_fee_per_gas,
            accounts,
            config.rules,
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .run()
        .await
    }
}
