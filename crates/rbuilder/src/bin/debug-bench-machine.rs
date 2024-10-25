//! App to benchmark/test the tx block execution.
//! This only works when reth node is stopped and the chain moved forward form its synced state
//! It downloads block aftre the last one synced and re-executes all the txs in it.
use alloy_provider::Provider;
use clap::Parser;
use eyre::Context;
use itertools::Itertools;
use rbuilder::{
    building::{BlockBuildingContext, BlockState, PartialBlock, PartialBlockFork},
    live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig, config::Config},
    utils::{extract_onchain_block_txs, find_suggested_fee_recipient, http_provider},
};
use reth::providers::BlockNumReader;
use reth_payload_builder::database::CachedReads;
use reth_provider::StateProvider;
use std::{path::PathBuf, sync::Arc, time::Instant};
use tracing::{debug, info};

#[derive(Parser, Debug)]
struct Cli {
    #[clap(long, help = "bench iterations", default_value = "20")]
    iters: usize,
    #[clap(
        long,
        help = "external block provider",
        env = "RPC_URL",
        default_value = "http://127.0.0.1:8545"
    )]
    rpc_url: String,
    #[clap(long, help = "Config file path", env = "RBUILDER_CONFIG")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    let config: Config = load_config_toml_and_env(cli.config)?;
    config.base_config().setup_tracing_subscriber()?;

    let rpc = http_provider(cli.rpc_url.parse()?);

    let chain_spec = config.base_config().chain_spec()?;

    let provider_factory = config.base_config().create_provider_factory()?;

    let last_block = provider_factory.last_block_number()?;

    let onchain_block = rpc
        .get_block_by_number((last_block + 1).into(), true)
        .await?
        .ok_or_else(|| eyre::eyre!("block not found on rpc"))?;

    let txs = extract_onchain_block_txs(&onchain_block)?;
    let suggested_fee_recipient = find_suggested_fee_recipient(&onchain_block, &txs);
    info!(
        "Block number: {}, txs: {}",
        onchain_block.header.number,
        txs.len()
    );

    let coinbase = onchain_block.header.miner;

    let ctx = BlockBuildingContext::from_onchain_block(
        onchain_block,
        chain_spec,
        None,
        Default::default(),
        coinbase,
        suggested_fee_recipient,
        None,
    );

    let state_provider = Arc::<dyn StateProvider>::from(
        provider_factory
            .provider_factory_unchecked()
            .history_by_block_number(last_block)?,
    );

    let mut build_times_ms = Vec::new();
    let mut finalize_time_ms = Vec::new();
    let mut cached_reads = Some(CachedReads::default());
    for _ in 0..cli.iters {
        let ctx = ctx.clone();
        let txs = txs.clone();
        let state_provider = state_provider.clone();
        let factory = provider_factory.clone();
        let config = config.clone();
        let root_hash_config = config.base_config.live_root_hash_config()?;
        let (new_cached_reads, build_time, finalize_time) =
            tokio::task::spawn_blocking(move || -> eyre::Result<_> {
                let partial_block = PartialBlock::new(true, None);
                let mut state = BlockState::new_arc(state_provider)
                    .with_cached_reads(cached_reads.unwrap_or_default());

                let build_time = Instant::now();

                let mut cumulative_gas_used = 0;
                let mut cumulative_blob_gas_used = 0;
                for (idx, tx) in txs.into_iter().enumerate() {
                    let result = {
                        let mut fork = PartialBlockFork::new(&mut state);
                        fork.commit_tx(&tx, &ctx, cumulative_gas_used, 0, cumulative_blob_gas_used)?
                            .with_context(|| {
                                format!("Failed to commit tx: {} {:?}", idx, tx.hash())
                            })?
                    };
                    cumulative_gas_used += result.gas_used;
                    cumulative_blob_gas_used += result.blob_gas_used;
                }

                let build_time = build_time.elapsed();

                let finalize_time = Instant::now();
                let finalized_block = partial_block.finalize(
                    &mut state,
                    &ctx,
                    factory.clone(),
                    root_hash_config.clone(),
                    config.base_config().root_hash_task_pool()?,
                )?;
                let finalize_time = finalize_time.elapsed();

                debug!(
                    "Calculated root hash: {:?}",
                    finalized_block.sealed_block.state_root
                );

                Ok((finalized_block.cached_reads, build_time, finalize_time))
            })
            .await??;

        cached_reads = Some(new_cached_reads);
        build_times_ms.push(build_time.as_millis());
        finalize_time_ms.push(finalize_time.as_millis());
    }
    report_time_data("build", &build_times_ms);
    report_time_data("finalize", &finalize_time_ms);

    Ok(())
}

fn report_time_data(action: &str, data: &[u128]) {
    let mean = data.iter().sum::<u128>() as f64 / data.len() as f64;
    let median = *data.iter().sorted().nth(data.len() / 2).unwrap();
    let max = *data.iter().max().unwrap();
    let min = *data.iter().min().unwrap();

    tracing::info!(
        "{} (us): mean: {}, median: {}, max: {}, min: {}",
        action,
        mean,
        median,
        max,
        min,
    );
}
