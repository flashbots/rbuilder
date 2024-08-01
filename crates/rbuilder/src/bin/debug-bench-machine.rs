//! App to benchmark/test the tx block execution.
//! It loads the last landed block and re-executes all the txs in it.
use alloy_primitives::{B256, U256};
use clap::Parser;
use itertools::Itertools;
use rbuilder::{
    building::{
        sim::simulate_all_orders_with_sim_tree, BlockBuildingContext, BlockState, PartialBlock,
    },
    live_builder::{base_config::load_config_toml_and_env, cli::LiveBuilderConfig, config::Config},
    primitives::{MempoolTx, Order, TransactionSignedEcRecoveredWithBlobs},
    roothash::RootHashMode,
    utils::{default_cfg_env, Signer},
};
use reth::{
    payload::PayloadId,
    providers::{BlockNumReader, BlockReader},
};
use reth_payload_builder::{database::CachedReads, EthPayloadBuilderAttributes};
use revm_primitives::{BlobExcessGasAndPrice, BlockEnv, SpecId};
use std::{path::PathBuf, time::Instant};

#[derive(Parser, Debug)]
struct Cli {
    #[clap(long, help = "bench iterations", default_value = "20")]
    iters: usize,
    #[clap(help = "Config file path")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    let config: Config = load_config_toml_and_env(cli.config)?;
    config.base_config().setup_tracing_subsriber()?;

    let chain = config.base_config().chain_spec()?;

    let factory = config
        .base_config()
        .provider_factory()?
        .provider_factory_unchecked();

    let last_block = factory.last_block_number()?;
    let block_data = factory
        .block_by_number(last_block)?
        .ok_or_else(|| eyre::eyre!("Block not found"))?;

    let signer = Signer::try_from_secret(B256::random())?;

    let cfg_env = default_cfg_env(&chain, block_data.timestamp);

    let ctx = BlockBuildingContext {
        block_env: BlockEnv {
            number: U256::from(block_data.number),
            coinbase: signer.address,
            timestamp: U256::from(block_data.timestamp),
            gas_limit: U256::from(block_data.gas_limit),
            basefee: U256::from(block_data.base_fee_per_gas.unwrap_or_default()),
            difficulty: Default::default(),
            prevrandao: Some(block_data.difficulty.into()),
            blob_excess_gas_and_price: block_data.excess_blob_gas.map(BlobExcessGasAndPrice::new),
        },
        initialized_cfg: cfg_env,
        attributes: EthPayloadBuilderAttributes {
            id: PayloadId::new([0u8; 8]),
            parent: block_data.parent_hash,
            timestamp: block_data.timestamp,
            suggested_fee_recipient: Default::default(),
            prev_randao: Default::default(),
            withdrawals: block_data.withdrawals.clone().unwrap_or_default(),
            parent_beacon_block_root: block_data.parent_beacon_block_root,
        },
        chain_spec: chain.clone(),
        builder_signer: Some(signer),
        extra_data: Vec::new(),
        blocklist: Default::default(),
        excess_blob_gas: block_data.excess_blob_gas,
        spec_id: SpecId::LATEST,
    };

    // Get the landed orders (all Order::Tx) from the block
    let orders = block_data
        .body
        .iter()
        .map(|tx| {
            let tx = tx
                .try_ecrecovered()
                .ok_or_else(|| eyre::eyre!("Failed to recover tx"))?;
            let tx = TransactionSignedEcRecoveredWithBlobs {
                tx,
                blobs_sidecar: Default::default(),
                metadata: Default::default(),
            };
            let tx = MempoolTx::new(tx);
            Ok::<_, eyre::Error>(Order::Tx(tx))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let state_provider = factory.history_by_block_number(block_data.number - 1)?;
    let (sim_orders, _) = simulate_all_orders_with_sim_tree(factory.clone(), &ctx, &orders, false)?;

    tracing::info!(
        "Block: {}, simulated orders: {}",
        block_data.number,
        sim_orders.len()
    );

    let mut build_times_mus = Vec::new();
    let mut finalize_time_mus = Vec::new();
    let mut cached_reads = Some(CachedReads::default());
    for _ in 0..cli.iters {
        let mut partial_block = PartialBlock::new(true, None);
        let mut block_state =
            BlockState::new(&state_provider).with_cached_reads(cached_reads.unwrap_or_default());
        let build_time = Instant::now();
        partial_block.pre_block_call(&ctx, &mut block_state)?;
        for order in &sim_orders {
            let _ = partial_block.commit_order(order, &ctx, &mut block_state)?;
        }
        let build_time = build_time.elapsed();

        let finalize_time = Instant::now();
        let finalized_block = partial_block.finalize(
            block_state,
            &ctx,
            factory.clone(),
            RootHashMode::IgnoreParentHash,
            config.base_config().root_hash_task_pool()?,
        )?;
        let finalize_time = finalize_time.elapsed();

        cached_reads = Some(finalized_block.cached_reads);

        build_times_mus.push(build_time.as_micros());
        finalize_time_mus.push(finalize_time.as_micros());
    }
    report_time_data("build", &build_times_mus);
    report_time_data("finalize", &finalize_time_mus);

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
