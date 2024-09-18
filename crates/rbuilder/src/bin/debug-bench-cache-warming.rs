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
use std::{path::PathBuf, sync::Arc, time::Instant, fs::{File, OpenOptions}, io::{Write, BufWriter}};
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
    let tx_count = txs.len();  // Store the transaction count
    let suggested_fee_recipient = find_suggested_fee_recipient(&onchain_block, &txs);
    info!(
        "Block number: {}, txs: {}",
        onchain_block.header.number,
        tx_count
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

    let block_number = last_block;
    let mut results = Vec::new();

    // Run the slow finalize benchmark once
    let (_, slow_finalize_times_ms) = run_benchmark(
        cli.iters,
        &txs,
        &ctx,
        &state_provider,
        &factory,
        &config,
        RethSparseTrieSharedCache::default(), // Use a default cache
        true, // Don't use sparse trie
    ).await?;

    report_time_data("slow_finalize", &slow_finalize_times_ms);

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
            false,
        ).await?;

        report_time_data(&format!("build"), &build_times_ms);
        report_time_data(&format!("finalize"), &finalize_times_ms);
        
        // Store results for CSV
        results.push((percentage, build_times_ms, finalize_times_ms));
    }

    // Save results to CSV, now including slow finalize times
    save_results_to_csv(block_number, tx_count, &results, &slow_finalize_times_ms)?;

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
    dont_use_sparse_trie: bool,
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
        let mut root_hash_config = config.base_config.live_root_hash_config()?;
        if dont_use_sparse_trie {
            root_hash_config.use_sparse_trie = false;
        }

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

fn save_results_to_csv(block_number: u64, tx_count: usize, results: &[(usize, Vec<u128>, Vec<u128>)], slow_finalize_times: &[u128]) -> eyre::Result<()> {
    let file_name = "benchmark_results.csv";
    let file_exists = std::path::Path::new(file_name).exists();

    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .append(true)
        .open(file_name)?;

    let mut writer = BufWriter::new(file);

    if !file_exists {
        // Write header if the file is newly created
        writeln!(writer, "block_number,tx_count,{},slow_finalize_median,slow_finalize_mean,slow_finalize_min,slow_finalize_max", 
            (10..=100).step_by(10).flat_map(|p| {
                [format!("{}_build_median", p), 
                 format!("{}_build_mean", p), 
                 format!("{}_build_min", p), 
                 format!("{}_build_max", p),
                 format!("{}_finalize_median", p), 
                 format!("{}_finalize_mean", p), 
                 format!("{}_finalize_min", p), 
                 format!("{}_finalize_max", p)]
            }).join(",")
        )?;
    }

    // Write data
    write!(writer, "{},{}", block_number+1, tx_count)?;
    for (percentage, build_times, finalize_times) in results {
        let build_stats = calculate_stats(build_times);
        let finalize_stats = calculate_stats(finalize_times);
        write!(writer, ",{},{},{},{},{},{},{},{}", 
            build_stats.median, build_stats.mean, build_stats.min, build_stats.max,
            finalize_stats.median, finalize_stats.mean, finalize_stats.min, finalize_stats.max
        )?;
    }
    
    // Write slow finalize stats
    let slow_finalize_stats = calculate_stats(slow_finalize_times);
    write!(writer, ",{},{},{},{}", 
        slow_finalize_stats.median, slow_finalize_stats.mean, slow_finalize_stats.min, slow_finalize_stats.max
    )?;
    
    writeln!(writer)?;

    writer.flush()?;
    Ok(())
}

struct Stats {
    median: f64,
    mean: f64,
    min: u128,
    max: u128,
}

fn calculate_stats(data: &[u128]) -> Stats {
    let mut sorted_data = data.to_vec();
    sorted_data.sort_unstable();

    let median = if data.len() % 2 == 0 {
        (sorted_data[data.len() / 2 - 1] + sorted_data[data.len() / 2]) as f64 / 2.0
    } else {
        sorted_data[data.len() / 2] as f64
    };

    let mean = data.iter().sum::<u128>() as f64 / data.len() as f64;
    let min = *data.iter().min().unwrap();
    let max = *data.iter().max().unwrap();

    Stats { median, mean, min, max }
}
