use clap::Parser;
use eyre::Result;
use monitoring::Monitoring;
use reth::providers::CanonStateSubscriptions;
use reth::CliRunner;
use reth_optimism_cli::{chainspec::OpChainSpecParser, Cli};
use reth_optimism_node::node::OpAddOnsBuilder;
use reth_optimism_node::OpNode;
use tokio::runtime::Runtime;

#[cfg(feature = "flashblocks")]
use payload_builder::CustomOpPayloadBuilder;
#[cfg(not(feature = "flashblocks"))]
use payload_builder_vanilla::CustomOpPayloadBuilder;
use reth_transaction_pool::TransactionPool;

/// CLI argument parsing.
pub mod args;
pub mod generator;
#[cfg(test)]
mod integration;
mod metrics;
mod monitor_tx_pool;
mod monitoring;
#[cfg(feature = "flashblocks")]
pub mod payload_builder;
#[cfg(not(feature = "flashblocks"))]
mod payload_builder_vanilla;
mod primitives;
#[cfg(feature = "telemetry")]
mod telemetry;
#[cfg(test)]
mod tester;
mod tx_signer;
use monitor_tx_pool::monitor_tx_pool;

fn main() -> Result<()> {
    let cli = Cli::<OpChainSpecParser, args::OpRbuilderArgs>::parse();

    // Create runtime in the outer scope
    let rt = Runtime::new()?;
    // let _guard = rt.enter();

    #[cfg(feature = "telemetry")]
    let mut provider =
        rt.block_on(async { telemetry::TelemetryControl::init_from_commands(&cli.command) })?;

    let runner = CliRunner::from_runtime(rt);

    cli.with_runner(runner, |builder, builder_args| async move {
        // Wrap the main execution in a span
        let span = tracing::info_span!("op-rbuilder");
        let _enter = span.enter();

        let rollup_args = builder_args.rollup_args;

        let op_node = OpNode::new(rollup_args.clone());
        let handle = builder
            .with_types::<OpNode>()
            .with_components(op_node.components().payload(CustomOpPayloadBuilder::new(
                builder_args.builder_signer,
                builder_args.flashblocks_ws_url,
                builder_args.chain_block_time,
                builder_args.flashblock_block_time,
            )))
            .with_add_ons(
                OpAddOnsBuilder::default()
                    .with_sequencer(rollup_args.sequencer.clone())
                    .with_enable_tx_conditional(rollup_args.enable_tx_conditional)
                    .build(),
            )
            .on_node_started(move |ctx| {
                let new_canonical_blocks = ctx.provider().canonical_state_stream();
                let builder_signer = builder_args.builder_signer;

                if builder_args.log_pool_transactions {
                    tracing::info!("Logging pool transactions");
                    ctx.task_executor.spawn_critical(
                        "txlogging",
                        Box::pin(async move {
                            monitor_tx_pool(ctx.pool.all_transactions_event_listener()).await;
                        }),
                    );
                }

                ctx.task_executor.spawn_critical(
                    "monitoring",
                    Box::pin(async move {
                        let monitoring = Monitoring::new(builder_signer);
                        let _ = monitoring.run_with_stream(new_canonical_blocks).await;
                    }),
                );

                Ok(())
            })
            .launch()
            .await?;

        handle.node_exit_future.await?;

        Ok(())
    })?;

    Ok(())
}
