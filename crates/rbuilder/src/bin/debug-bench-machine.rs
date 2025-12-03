//! App to benchmark/test the tx block execution.
//! This only works when reth node is stopped and the chain moved forward from its synced state
//! It downloads block after the last one synced and re-executes all the txs in it.
use alloy_consensus::TxEnvelope;
use alloy_eips::Decodable2718;
use alloy_primitives::address;
use alloy_provider::Provider;
use alloy_rpc_types_beacon::relay::SubmitBlockRequest as AlloySubmitBlockRequest;
use clap::Parser;
use eyre::Context;
use itertools::Itertools;
use rbuilder::{
    building::{
        BlockBuildingContext, BlockBuildingSpaceState, BlockState, FinalizeAdjustmentState,
        PartialBlock, PartialBlockFork, ThreadBlockBuildingContext,
    },
    live_builder::{cli::LiveBuilderConfig, config::Config},
    provider::StateProviderFactory,
    utils::{
        extract_onchain_block_txs, find_suggested_fee_recipient, http_provider,
        mevblocker::get_mevblocker_price, Signer,
    },
};
use rbuilder_config::load_toml_config;
use rbuilder_primitives::mev_boost::SubmitBlockRequest;
use reth_primitives_traits::SignerRecoverable;
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
    #[clap(
        long,
        help = "Path to submit block request to replay to use instead of the onchain block"
    )]
    submit_block_request_json: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    let config: Config = load_toml_config(cli.config)?;
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

    let onchain_block = if let Some(submit_block_request_json) = cli.submit_block_request_json {
        let mut block = read_execution_payload_from_json(submit_block_request_json)?;
        // without parent_beacon_block_root we can't build block and its not available in submit_block_request_json
        block.header.parent_beacon_block_root = onchain_block.header.parent_beacon_block_root;
        block
    } else {
        onchain_block
    };

    let txs = extract_onchain_block_txs(&onchain_block)?;
    let suggested_fee_recipient = find_suggested_fee_recipient(&onchain_block, &txs);
    info!(
        "Block number: {}, txs: {}",
        onchain_block.header.number,
        txs.len()
    );

    let coinbase = onchain_block.header.beneficiary;

    let parent_num_hash = onchain_block.header.parent_num_hash();
    let mev_blocker_price =
        get_mevblocker_price(provider_factory.history_by_block_hash(parent_num_hash.hash)?)?;
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
        mev_blocker_price,
    );

    let state_provider = Arc::<dyn StateProvider>::from(
        provider_factory
            .provider_factory_unchecked()
            .history_by_block_number(last_block)?,
    );

    let mut build_times_ms = Vec::new();
    let mut finalize_time_ms = Vec::new();
    for _ in 0..cli.iters {
        let mut ctx = ctx.clone();
        // add one random empty account and real adjusted fee payer to hit a code path for creating bid adjustment data in finalization
        ctx.adjustment_fee_payers = [
            address!("a41772428931BE72C28011f114A15B4211DFdfE5"),
            address!("59CadF9199248b50d40a6891c9E329eA13a88d31"),
        ]
        .into_iter()
        .collect();
        let txs = txs.clone();
        let state_provider = state_provider.clone();
        let (build_time, finalize_time) =
            tokio::task::spawn_blocking(move || -> eyre::Result<_> {
                let mut partial_block = PartialBlock::new(true);
                let mut state = BlockState::new_arc(state_provider);
                let mut local_ctx = ThreadBlockBuildingContext::default();

                let mut finalize_adjustment_state = FinalizeAdjustmentState::default();

                let build_time = Instant::now();

                partial_block.pre_block_call(&ctx, &mut local_ctx, &mut state)?;

                let mut space_state = BlockBuildingSpaceState::ZERO;
                for (idx, tx) in txs.into_iter().enumerate() {
                    let result = {
                        let mut fork = PartialBlockFork::new(&mut state, &ctx, &mut local_ctx);

                        fork.commit_tx(&tx, space_state)?.with_context(|| {
                            format!("Failed to commit tx: {} {:?}", idx, tx.hash())
                        })?
                    };
                    space_state.use_space(result.space_used());
                    partial_block.executed_tx_infos.push(result.tx_info);
                }

                let build_time = build_time.elapsed();

                let finalize_time = Instant::now();
                let finalized_block = partial_block.finalize(
                    &mut state,
                    &ctx,
                    &mut local_ctx,
                    false,
                    &mut finalize_adjustment_state,
                )?;
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

fn read_execution_payload_from_json(path: PathBuf) -> eyre::Result<alloy_rpc_types::Block> {
    let req = std::fs::read_to_string(&path)?;
    let req: SubmitBlockRequest = serde_json::from_str(&req)?;
    let block_raw = match req.request.as_ref() {
        AlloySubmitBlockRequest::Capella(req) => req.execution_payload.clone().into_block_raw()?,
        AlloySubmitBlockRequest::Fulu(req) => req.execution_payload.clone().into_block_raw()?,
        AlloySubmitBlockRequest::Deneb(req) => req.execution_payload.clone().into_block_raw()?,
        AlloySubmitBlockRequest::Electra(req) => req.execution_payload.clone().into_block_raw()?,
    };
    let rpc_block = alloy_rpc_types::Block::from_consensus(block_raw, None);
    let rpc_block = rpc_block.try_map_transactions(|bytes| -> eyre::Result<_> {
        let envelope = TxEnvelope::decode_2718(&mut bytes.as_ref())?;
        let recovered = envelope.try_into_recovered()?;
        Ok(alloy_rpc_types::Transaction::from_transaction(
            recovered,
            alloy_rpc_types::TransactionInfo::default(),
        ))
    })?;
    Ok(rpc_block)
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
