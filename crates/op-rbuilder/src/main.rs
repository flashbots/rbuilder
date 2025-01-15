use args::OpRbuilderArgs;
use clap::Parser;
use generator::EmptyBlockPayloadJobGenerator;
use payload_builder::OpPayloadBuilder as FBPayloadBuilder;
use payload_builder_vanilla::OpPayloadBuilderVanilla;
use rbuilder::utils::Signer;
use reth::{
    builder::{components::PayloadServiceBuilder, node::FullNodeTypes, BuilderContext},
    payload::PayloadBuilderHandle,
    providers::CanonStateSubscriptions,
    transaction_pool::TransactionPool,
};
use reth::{
    builder::{engine_tree_config::TreeConfig, EngineNodeLauncher},
    providers::providers::BlockchainProvider2,
};
use reth_basic_payload_builder::BasicPayloadJobGeneratorConfig;
use reth_node_api::NodeTypesWithEngine;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_cli::{chainspec::OpChainSpecParser, Cli};
use reth_optimism_evm::OpEvmConfig;
use reth_optimism_node::OpEngineTypes;
use reth_optimism_node::{node::OpAddOns, OpNode};
use reth_payload_builder::PayloadBuilderService;

/// CLI argument parsing.
pub mod args;

pub mod generator;
pub mod payload_builder;
mod payload_builder_vanilla;

#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct CustomPayloadBuilder {
    builder_secret_key: Option<Signer>,
}

impl CustomPayloadBuilder {
    pub fn new(builder_secret_key: Option<Signer>) -> Self {
        Self { builder_secret_key }
    }
}

impl<Node, Pool> PayloadServiceBuilder<Node, Pool> for CustomPayloadBuilder
where
    Node:
        FullNodeTypes<Types: NodeTypesWithEngine<Engine = OpEngineTypes, ChainSpec = OpChainSpec>>,
    Pool: TransactionPool + Unpin + 'static,
{
    async fn spawn_payload_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypesWithEngine>::Engine>> {
        tracing::info!("Spawning a custom payload builder");
        let _fb_builder = FBPayloadBuilder::new(OpEvmConfig::new(ctx.chain_spec()));
        let vanilla_builder = OpPayloadBuilderVanilla::new(OpEvmConfig::new(ctx.chain_spec()), self.builder_secret_key);
        let payload_job_config = BasicPayloadJobGeneratorConfig::default();

        let payload_generator = EmptyBlockPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            pool,
            ctx.task_executor().clone(),
            payload_job_config,
            // FBPayloadBuilder::new(OpEvmConfig::new(ctx.chain_spec())),
            vanilla_builder,
        );

        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical("custom payload builder service", Box::pin(payload_service));

        Ok(payload_builder)
    }
}

fn main() {
    Cli::<OpChainSpecParser, OpRbuilderArgs>::parse()
        .run(|builder, op_rbuilder_args| async move {
            if op_rbuilder_args.rollup_args.experimental {
                tracing::warn!(target: "reth::cli", "Experimental engine is default now, and the --engine.experimental flag is deprecated. To enable the legacy functionality, use --engine.legacy.");
            }
            let use_legacy_engine = op_rbuilder_args.rollup_args.legacy;
            let sequencer_http_arg = op_rbuilder_args.rollup_args.sequencer_http.clone();
            let builder_signer = op_rbuilder_args.builder_secret_key()?;
            match use_legacy_engine {
                false => {
                    let engine_tree_config = TreeConfig::default()
                        .with_persistence_threshold(op_rbuilder_args.rollup_args.persistence_threshold)
                        .with_memory_block_buffer_target(op_rbuilder_args.rollup_args.memory_block_buffer_target);
                    let handle = builder
                        .with_types_and_provider::<OpNode, BlockchainProvider2<_>>()
                        .with_components(
                            OpNode::components(op_rbuilder_args.rollup_args).payload(CustomPayloadBuilder::new(builder_signer)),
                        )
                        .with_add_ons(OpAddOns::new(sequencer_http_arg))
                        .launch_with_fn(|builder| {
                            let launcher = EngineNodeLauncher::new(
                                builder.task_executor().clone(),
                                builder.config().datadir(),
                                engine_tree_config,
                            );
                            builder.launch_with(launcher)
                        })
                        .await?;

                    handle.node_exit_future.await
                },
                true => {
                    let handle =
                        builder.node(OpNode::new(op_rbuilder_args.rollup_args.clone())).launch().await?;

                    handle.node_exit_future.await
                },
            }
        })
        .unwrap();
}
