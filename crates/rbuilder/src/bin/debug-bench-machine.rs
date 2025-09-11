//! App to benchmark/test the tx block execution.
//! This only works when reth node is stopped and the chain moved forward from its synced state
//! It downloads block after the last one synced and re-executes all the txs in it.
use alloy_provider::Provider;
use clap::Parser;
use eyre::Context;
use itertools::Itertools;
use rbuilder::{
    building::{
        BlockBuildingContext, BlockBuildingSpaceState, BlockState, PartialBlock, PartialBlockFork,
        ThreadBlockBuildingContext,
    },
    live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig, config::Config},
    provider::StateProviderFactory,
    utils::{extract_onchain_block_txs, find_suggested_fee_recipient, http_provider, Signer},
};
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

    let provider_factory = config.base_config().create_reth_provider_factory(false)?;

    let last_block = provider_factory.last_block_number()?;

    let onchain_block = rpc
        .get_block_by_number((last_block + 1).into())
        .full()
        .await?
        .ok_or_else(|| eyre::eyre!("block not found on rpc"))?;

    let txs = extract_onchain_block_txs(&onchain_block)?;
    let suggested_fee_recipient = find_suggested_fee_recipient(&onchain_block, &txs);
    info!(
        "Block number: {}, txs: {}",
        onchain_block.header.number,
        txs.len()
    );

    let coinbase = onchain_block.header.beneficiary;

    let parent_num_hash = onchain_block.header.parent_num_hash();
    let ctx = BlockBuildingContext::from_onchain_block(
        onchain_block,
        chain_spec,
        None,
        Default::default(),
        coinbase,
        suggested_fee_recipient,
        Signer::random(),
        Arc::from(provider_factory.root_hasher(parent_num_hash)?),
        config.base_config().evm_caching_enable,
    );

    let state_provider = Arc::<dyn StateProvider>::from(
        provider_factory
            .provider_factory_unchecked()
            .history_by_block_number(last_block)?,
    );

    let mut build_times_ms = Vec::new();
    let mut finalize_time_ms = Vec::new();
    for _ in 0..cli.iters {
        let ctx = ctx.clone();
        let txs = txs.clone();
        let state_provider = state_provider.clone();
        let (build_time, finalize_time) =
            tokio::task::spawn_blocking(move || -> eyre::Result<_> {
                let partial_block = PartialBlock::new(true);
                let mut state = BlockState::new_arc(state_provider);
                let mut local_ctx = ThreadBlockBuildingContext::default();

                let build_time = Instant::now();

                let mut space_state = BlockBuildingSpaceState::ZERO;
                for (idx, tx) in txs.into_iter().enumerate() {
                    let result = {
                        let mut fork = PartialBlockFork::new(&mut state, &ctx, &mut local_ctx);
                        fork.commit_tx(&tx, space_state)?.with_context(|| {
                            format!("Failed to commit tx: {} {:?}", idx, tx.hash())
                        })?
                    };
                    space_state.use_space(result.space_used(), result.blob_gas_used);
                }

                let build_time = build_time.elapsed();

                let finalize_time = Instant::now();
                let finalized_block = partial_block.finalize(state, &ctx, &mut local_ctx)?;
                let finalize_time = finalize_time.elapsed();

                debug!(
                    "Calculated root hash: {:?}",
                    finalized_block.sealed_block.state_root
                );

                Ok((build_time, finalize_time))
            })
            .await??;

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
