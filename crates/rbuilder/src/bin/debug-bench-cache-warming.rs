//! App to benchmark/test the tx block execution.
//! This only works when reth node is stopped and the chain moved forward form its synced state
//! It downloads block aftre the last one synced and re-executes all the txs in it.
use alloy_provider::Provider;
use clap::Parser;
use eth_sparse_mpt::reth_sparse_trie::RethSparseTrieSharedCache;
use eyre::Context;
use itertools::Itertools;
use rbuilder::{
    building::{BlockBuildingContext, BlockState, PartialBlock, PartialBlockFork}, live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig, config::Config}, primitives::TransactionSignedEcRecoveredWithBlobs, utils::{extract_onchain_block_txs, find_suggested_fee_recipient, http_provider}
};
use reth::providers::BlockNumReader;
use reth_db::DatabaseEnv;
use reth_payload_builder::database::CachedReads;
use reth_provider::{ProviderFactory, StateProvider};
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
    #[clap(long, help = "Benchmark cache warm", default_value = "false")]
    bench_cache_warm: bool,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    let config: Config = load_config_toml_and_env(cli.config)?;
    config.base_config().setup_tracing_subsriber()?;

    let rpc = http_provider(cli.rpc_url.parse()?);

    let chain_spec = config.base_config().chain_spec()?;

    let factory = config
        .base_config()
        .provider_factory()?
        .provider_factory_unchecked();

    let last_block = factory.last_block_number()?;

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

    // let signer = Signer::try_from_secret(B256::random())?;

    let state_provider =
        Arc::<dyn StateProvider>::from(factory.history_by_block_number(last_block)?);

    let percentages = vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100];

    for percentage in percentages {
        info!("Benchmarking with {}% pre-warmed cache and {} txs", percentage, percentage * txs.len() / 100);
        
        let sparse_mpt_cache_pre_warm = pre_warm_cache(
            percentage,
            &txs,
            &ctx,
            &state_provider,
            &factory,
            &config,
        ).await?;

        let (build_times_ms, finalize_times_ms) = run_benchmark(
            cli.iters,
            &txs,
            &ctx,
            &state_provider,
            &factory,
            &config,
            sparse_mpt_cache_pre_warm,
        ).await?;

        report_time_data(&format!("build"), &build_times_ms);
        report_time_data(&format!("finalize"), &finalize_times_ms);
    }

    Ok(())
}

async fn pre_warm_cache(
    percentage: usize,
    txs: &Vec<TransactionSignedEcRecoveredWithBlobs>,
    ctx: &BlockBuildingContext,
    state_provider: &Arc<dyn StateProvider>,
    factory: &ProviderFactory<Arc<DatabaseEnv>>,
    config: &Config,
) -> eyre::Result<RethSparseTrieSharedCache> {
    let ctx_clone = ctx.clone();
    let txs_clone = txs.clone();
    let state_provider_clone = state_provider.clone();
    let factory_clone = factory.clone();
    let config_clone = config.clone();
    let root_hash_config = config_clone.base_config.live_root_hash_config()?;

    tokio::task::spawn_blocking(move || -> eyre::Result<_> {
        let txs_to_execute = txs_clone.iter().take(txs_clone.len() * percentage / 100).collect_vec();
        // info!("Pre-warming cache with {} transactions", txs_to_execute.len());
        
        let partial_block = PartialBlock::new(true, None);
        let mut state = BlockState::new_arc(state_provider_clone);

        let mut cumulative_gas_used = 0;
        let mut cumulative_blob_gas_used = 0;
        for (idx, tx) in txs_to_execute.into_iter().enumerate() {
            let result = {
                let mut fork = PartialBlockFork::new(&mut state);
                fork.commit_tx(tx, &ctx_clone, cumulative_gas_used, 0, cumulative_blob_gas_used)?
                    .with_context(|| format!("Failed to commit tx: {} {:?}", idx, tx.hash()))?
            };
            cumulative_gas_used += result.gas_used;
            cumulative_blob_gas_used += result.blob_gas_used;
        }

        let _ = partial_block.finalize(
            &mut state,
            &ctx_clone,
            factory_clone.clone(),
            root_hash_config.clone(),
            config_clone.base_config().root_hash_task_pool()?,
            None,
        )?;
        let deep_cloned_cache: RethSparseTrieSharedCache = ctx_clone.shared_sparse_mpt_cache.deep_clone();
        Ok(deep_cloned_cache)
    }).await?
}

async fn run_benchmark(
    iters: usize,
    txs: &Vec<TransactionSignedEcRecoveredWithBlobs>,
    ctx: &BlockBuildingContext,
    state_provider: &Arc<dyn StateProvider>,
    factory: &ProviderFactory<Arc<DatabaseEnv>>,
    config: &Config,
    sparse_mpt_cache_pre_warm: RethSparseTrieSharedCache,
) -> eyre::Result<(Vec<u128>, Vec<u128>)> {
    let mut build_times_ms = Vec::new();
    let mut finalize_times_ms = Vec::new();
    let mut cached_reads = Some(CachedReads::default());

    for _ in 0..iters {
        let mut ctx = ctx.clone();
        let txs = txs.clone();
        let state_provider = state_provider.clone();
        let factory = factory.clone();
        let config = config.clone();
        let root_hash_config = config.base_config.live_root_hash_config()?;

        // Clone the pre-warmed cache for the stateroot hash calculation
        let sparse_mpt_cache_pre_warm_clone = sparse_mpt_cache_pre_warm.deep_clone();
    
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

                // Use the pre-warmed cache for the stateroot hash calculation
                ctx.over_ride_shared_sparse_mpt_cache(sparse_mpt_cache_pre_warm_clone);

                let finalize_time = Instant::now();
                let finalized_block = partial_block.finalize(
                    &mut state,
                    &ctx,
                    factory.clone(),
                    root_hash_config.clone(),
                    config.base_config().root_hash_task_pool()?,
                    None,
                )?;
                let finalize_time = finalize_time.elapsed();
                debug!("finalize_time: {:?}", finalize_time);

                Ok((finalized_block.cached_reads, build_time, finalize_time))
            })
            .await??;

        cached_reads = Some(new_cached_reads);
        build_times_ms.push(build_time.as_millis());
        finalize_times_ms.push(finalize_time.as_millis());
    }

    Ok((build_times_ms, finalize_times_ms))
}

fn report_time_data(action: &str, data: &[u128]) {
    let mean = data.iter().sum::<u128>() as f64 / data.len() as f64;
    let median = *data.iter().sorted().nth(data.len() / 2).unwrap();
    let max = *data.iter().max().unwrap();
    let min = *data.iter().min().unwrap();

    tracing::info!(
        "{} (ms): mean: {}, median: {}, max: {}, min: {}",
        action,
        mean,
        median,
        max,
        min,
    );
}
