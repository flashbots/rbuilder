use crate::generator::{BlockCell, PayloadBuilder};
use alloy_consensus::Header;
use clap::Parser;
use generator::EmptyBlockPayloadJobGenerator;
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
use reth_basic_payload_builder::BuildArguments;
use reth_basic_payload_builder::PayloadBuilder as X;
use reth_basic_payload_builder::{BasicPayloadJobGeneratorConfig, BuildOutcome};
use reth_chainspec::ChainSpecProvider;
use reth_evm::ConfigureEvm;
use reth_node_api::NodeTypesWithEngine;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_cli::{chainspec::OpChainSpecParser, Cli};
use reth_optimism_evm::OpEvmConfig;
use reth_optimism_node::OpEngineTypes;
use reth_optimism_node::{args::RollupArgs, node::OpAddOns, OpNode};
use reth_optimism_node::{OpBuiltPayload, OpPayloadBuilderAttributes};
use reth_payload_builder::PayloadBuilderError;
use reth_payload_builder::PayloadBuilderService;
use reth_provider::StateProviderFactory;

pub mod generator;

#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct CustomPayloadBuilder;

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
        let _conf = ctx.payload_builder_config();

        let payload_job_config = BasicPayloadJobGeneratorConfig::default();

        let payload_generator = EmptyBlockPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            pool,
            ctx.task_executor().clone(),
            payload_job_config,
            OpPayloadBuilder::new(OpEvmConfig::new(ctx.chain_spec())),
        );

        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical("custom payload builder service", Box::pin(payload_service));

        Ok(payload_builder)
    }
}

fn main() {
    Cli::<OpChainSpecParser, RollupArgs>::parse()
        .run(|builder, rollup_args| async move {
            if rollup_args.experimental {
                tracing::warn!(target: "reth::cli", "Experimental engine is default now, and the --engine.experimental flag is deprecated. To enable the legacy functionality, use --engine.legacy.");
            }
            let use_legacy_engine = rollup_args.legacy;
            let sequencer_http_arg = rollup_args.sequencer_http.clone();

            match use_legacy_engine {
                false => {
                    let engine_tree_config = TreeConfig::default()
                        .with_persistence_threshold(rollup_args.persistence_threshold)
                        .with_memory_block_buffer_target(rollup_args.memory_block_buffer_target);
                    let handle = builder
                        .with_types_and_provider::<OpNode, BlockchainProvider2<_>>()
                        .with_components(
                            OpNode::components(rollup_args).payload(CustomPayloadBuilder::default()),
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
                        builder.node(OpNode::new(rollup_args.clone())).launch().await?;

                    handle.node_exit_future.await
                },
            }
        })
        .unwrap();
}

#[derive(Clone)]
struct OpPayloadBuilder<EvmConfig> {
    inner: reth_optimism_payload_builder::OpPayloadBuilder<EvmConfig>,
}

impl<EvmConfig> OpPayloadBuilder<EvmConfig> {
    /// `OpPayloadBuilder` constructor.
    pub const fn new(evm_config: EvmConfig) -> Self {
        Self {
            inner: reth_optimism_payload_builder::OpPayloadBuilder::new(evm_config),
        }
    }
}

impl<EvmConfig, Pool, Client> PayloadBuilder<Pool, Client> for OpPayloadBuilder<EvmConfig>
where
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec = OpChainSpec>,
    Pool: TransactionPool,
    EvmConfig: ConfigureEvm<Header = Header>,
{
    type Attributes = OpPayloadBuilderAttributes;
    type BuiltPayload = OpBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<Pool, Client, Self::Attributes, Self::BuiltPayload>,
        best_payload: BlockCell<Self::BuiltPayload>,
    ) -> Result<(), PayloadBuilderError> {
        match self.inner.try_build(args)? {
            BuildOutcome::Better { payload, .. } => {
                best_payload.set(payload);
                Ok(())
            }
            _ => {
                tracing::warn!("No better payload found");
                Err(PayloadBuilderError::MissingPayload)
            }
        }
    }
}
