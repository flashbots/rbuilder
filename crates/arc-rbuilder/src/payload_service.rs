//! Payload service wiring that puts rbuilder between Malachite consensus and
//! the Arc node's own payload builder.
//!
//! Flow:
//! 1. Malachite sends `engine_forkchoiceUpdatedV3` with payload attributes.
//! 2. [`RbuilderJobGenerator::new_payload_job`] registers a payload cell with
//!    the rbuilder [`EnginePayloadRegistry`], pushes the slot to the rbuilder
//!    [`SlotSource`] channel and starts the regular Arc payload job as
//!    fallback.
//! 3. rbuilder builds blocks; every new best block is finalized and published
//!    into the cell (see `EnginePayloadSink` in rbuilder).
//! 4. `engine_getPayload` resolves the job: rbuilder's best block wins, the
//!    Arc builder's block is the fallback so consensus liveness never depends
//!    on rbuilder.

use alloy_primitives::B256;
use alloy_rpc_types_beacon::events::{PayloadAttributesData, PayloadAttributesEvent};
use alloy_rpc_types_engine::PayloadAttributes;
use rbuilder::{
    building::arc_support,
    chain::ChainSpec,
    live_builder::{
        block_output::engine_payload_sink::EnginePayloadRegistry,
        payload_events::{InternalPayloadId, MevBoostSlotData},
    },
};
use reth_basic_payload_builder::{BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig};
use reth_chainspec::EthChainSpec as _;
use reth_ethereum_engine_primitives::{EthBuiltPayload, EthPayloadBuilderAttributes};
use reth_evm::{ConfigureEvm as _, NextBlockEnvAttributes};
use reth_node_api::{FullNodeTypes, NodeTypes, PayloadTypes};
use reth_node_builder::{
    components::{PayloadBuilderBuilder, PayloadServiceBuilder},
    BuilderContext,
};
use reth_payload_builder::{
    KeepPayloadJobAlive, PayloadBuilderError, PayloadBuilderHandle, PayloadBuilderService,
    PayloadJob, PayloadJobGenerator, PayloadKind,
};
use reth_provider::{
    CanonStateSubscriptions, ChainSpecProvider, HeaderProvider, StateProviderFactory,
};
use reth_revm::database::StateProviderDatabase;
use reth_transaction_pool::TransactionPool;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

/// Handles shared between the payload service and the rbuilder instance.
#[derive(Debug, Clone)]
pub struct RbuilderBridge {
    pub registry: Arc<EnginePayloadRegistry>,
    pub slot_sender: mpsc::UnboundedSender<MevBoostSlotData>,
}

impl RbuilderBridge {
    pub fn new(slot_sender: mpsc::UnboundedSender<MevBoostSlotData>) -> Self {
        Self {
            registry: Arc::new(EnginePayloadRegistry::default()),
            slot_sender,
        }
    }
}

/// [`PayloadServiceBuilder`] that wraps the Arc payload builder ([`PB`]) with
/// the rbuilder bridge. Mirrors reth's `BasicPayloadServiceBuilder`, with the
/// job generator wrapped in [`RbuilderJobGenerator`].
#[derive(Debug)]
pub struct RbuilderPayloadServiceBuilder<PB> {
    inner: PB,
    bridge: RbuilderBridge,
}

impl<PB> RbuilderPayloadServiceBuilder<PB> {
    pub fn new(inner: PB, bridge: RbuilderBridge) -> Self {
        Self { inner, bridge }
    }
}

impl<Node, Pool, EvmCfg, PB> PayloadServiceBuilder<Node, Pool, EvmCfg>
    for RbuilderPayloadServiceBuilder<PB>
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            ChainSpec = ChainSpec,
            Primitives = reth::primitives::EthPrimitives,
            Payload: PayloadTypes<
                PayloadBuilderAttributes = EthPayloadBuilderAttributes,
                BuiltPayload = EthBuiltPayload,
            >,
        >,
    >,
    Pool: TransactionPool,
    EvmCfg: Send,
    PB: PayloadBuilderBuilder<Node, Pool, EvmCfg>,
    <PB::PayloadBuilder as reth_basic_payload_builder::PayloadBuilder>::Attributes:
        Unpin + Clone,
    <PB::PayloadBuilder as reth_basic_payload_builder::PayloadBuilder>::BuiltPayload:
        Unpin + Clone,
{
    async fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: EvmCfg,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypes>::Payload>> {
        let payload_builder = self.inner.build_payload_builder(ctx, pool, evm_config).await?;

        let conf = ctx.config().builder.clone();
        let payload_job_config = BasicPayloadJobGeneratorConfig::default()
            .interval(conf.interval)
            .deadline(conf.deadline)
            .max_payload_tasks(conf.max_payload_tasks);

        let inner_generator = BasicPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            payload_job_config,
            payload_builder,
        );

        let generator = RbuilderJobGenerator {
            inner: inner_generator,
            bridge: self.bridge,
            client: ctx.provider().clone(),
            chain_spec: ctx.chain_spec(),
        };

        let (payload_service, payload_service_handle) =
            PayloadBuilderService::new(generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical_task("payload builder service", Box::pin(payload_service));

        Ok(payload_service_handle)
    }
}

/// [`PayloadJobGenerator`] that forwards new payload jobs to rbuilder and
/// wraps the inner (Arc) jobs so resolution prefers rbuilder's block.
#[derive(Debug)]
pub struct RbuilderJobGenerator<Gen, Client> {
    inner: Gen,
    bridge: RbuilderBridge,
    client: Client,
    chain_spec: Arc<ChainSpec>,
}

impl<Gen, Client> RbuilderJobGenerator<Gen, Client>
where
    Client: StateProviderFactory + HeaderProvider<Header = alloy_consensus::Header>,
{
    /// Converts engine payload attributes into the slot data rbuilder
    /// consumes, computing the ProtocolConfig gas limit on parent state.
    fn slot_data(
        &self,
        attributes: &EthPayloadBuilderAttributes,
        internal_payload_id: InternalPayloadId,
    ) -> eyre::Result<MevBoostSlotData> {
        let parent_hash = attributes.parent;
        let parent_header = self
            .client
            .header(parent_hash)?
            .ok_or_else(|| eyre::eyre!("parent header {parent_hash} not found"))?;

        // EVM env for the ProtocolConfig system call on top of the parent
        // state (gas limit value is a placeholder, exactly like
        // ArcEvmConfig::builder_for_next_block does).
        let evm_env = rbuilder::chain::evm_config(self.chain_spec.clone()).next_evm_env(
            &parent_header,
            &NextBlockEnvAttributes {
                timestamp: attributes.timestamp,
                suggested_fee_recipient: attributes.suggested_fee_recipient,
                prev_randao: attributes.prev_randao,
                gas_limit: parent_header.gas_limit,
                withdrawals: Some(attributes.withdrawals.clone()),
                parent_beacon_block_root: attributes.parent_beacon_block_root,
                extra_data: Default::default(),
            },
        )?;
        let state = self.client.state_by_block_hash(parent_hash)?;
        let db = revm::database::State::builder()
            .with_database(StateProviderDatabase::new(state))
            .with_bundle_update()
            .build();
        let gas_limit = arc_support::expected_block_gas_limit(
            self.chain_spec.clone(),
            db,
            &parent_header,
            evm_env,
        );

        let payload_attributes_event = PayloadAttributesEvent {
            // Arc has no beacon chain; the version string is only used for
            // logging inside rbuilder.
            version: "arc".to_string(),
            data: PayloadAttributesData {
                proposal_slot: parent_header.number + 1,
                parent_block_root: B256::ZERO,
                parent_block_number: parent_header.number,
                parent_block_hash: parent_hash,
                proposer_index: 0,
                payload_attributes: PayloadAttributes {
                    timestamp: attributes.timestamp,
                    prev_randao: attributes.prev_randao,
                    suggested_fee_recipient: attributes.suggested_fee_recipient,
                    withdrawals: Some(attributes.withdrawals.clone().into_inner()),
                    parent_beacon_block_root: attributes.parent_beacon_block_root,
                },
            },
        };

        Ok(MevBoostSlotData {
            payload_attributes_event,
            suggested_gas_limit: gas_limit,
            relay_registrations: Arc::new(Default::default()),
            slot_data: rbuilder::live_builder::payload_events::relay_epoch_cache::SlotData {
                fee_recipient: attributes.suggested_fee_recipient,
                gas_limit,
                pubkey: Default::default(),
            },
            payload_id: internal_payload_id,
        })
    }
}

impl<Gen, Client> PayloadJobGenerator for RbuilderJobGenerator<Gen, Client>
where
    Gen: PayloadJobGenerator,
    Gen::Job: PayloadJob<
            PayloadAttributes = EthPayloadBuilderAttributes,
            BuiltPayload = EthBuiltPayload,
        > + Unpin,
    Client: StateProviderFactory
        + HeaderProvider<Header = alloy_consensus::Header>
        + ChainSpecProvider
        + Clone
        + 'static,
{
    type Job = RbuilderPayloadJob<Gen::Job>;

    fn new_payload_job(
        &self,
        attributes: EthPayloadBuilderAttributes,
    ) -> Result<Self::Job, PayloadBuilderError> {
        let payload_id = attributes.payload_id();
        let internal_id = u64::from_be_bytes(payload_id.0 .0);

        let inner = self.inner.new_payload_job(attributes.clone())?;

        let best_payload = self.bridge.registry.register(internal_id);
        match self.slot_data(&attributes, internal_id) {
            Ok(slot_data) => {
                info!(
                    %payload_id,
                    block = slot_data.block(),
                    parent_hash = %slot_data.parent_block_hash(),
                    gas_limit = slot_data.suggested_gas_limit,
                    chain = %self.chain_spec.chain_id(),
                    "Forwarding payload job to rbuilder"
                );
                if self.bridge.slot_sender.send(slot_data).is_err() {
                    warn!("rbuilder slot channel closed; building with fallback builder only");
                }
            }
            Err(err) => {
                warn!(?err, %payload_id, "Failed to build rbuilder slot data; building with fallback builder only");
            }
        }

        Ok(RbuilderPayloadJob {
            inner,
            registry: self.bridge.registry.clone(),
            internal_id,
            best_payload,
        })
    }

    fn on_new_state<N: reth_node_api::NodePrimitives>(
        &mut self,
        new_state: reth_provider::CanonStateNotification<N>,
    ) {
        self.inner.on_new_state(new_state)
    }
}

/// Payload job preferring rbuilder's published block over the inner (Arc
/// builder) job's payload.
#[derive(Debug)]
pub struct RbuilderPayloadJob<J> {
    inner: J,
    registry: Arc<EnginePayloadRegistry>,
    internal_id: InternalPayloadId,
    best_payload: watch::Receiver<Option<EthBuiltPayload>>,
}

impl<J> Drop for RbuilderPayloadJob<J> {
    fn drop(&mut self) {
        self.registry.unregister(self.internal_id);
    }
}

impl<J> Future for RbuilderPayloadJob<J>
where
    J: Future<Output = Result<(), PayloadBuilderError>> + Unpin,
{
    type Output = Result<(), PayloadBuilderError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.inner).poll(cx)
    }
}

impl<J> PayloadJob for RbuilderPayloadJob<J>
where
    J: PayloadJob<PayloadAttributes = EthPayloadBuilderAttributes, BuiltPayload = EthBuiltPayload>
        + Unpin,
{
    type PayloadAttributes = EthPayloadBuilderAttributes;
    type ResolvePayloadFuture = futures::future::Either<
        futures::future::Ready<Result<EthBuiltPayload, PayloadBuilderError>>,
        J::ResolvePayloadFuture,
    >;
    type BuiltPayload = EthBuiltPayload;

    fn best_payload(&self) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        if let Some(payload) = self.best_payload.borrow().clone() {
            return Ok(payload);
        }
        self.inner.best_payload()
    }

    fn payload_attributes(&self) -> Result<Self::PayloadAttributes, PayloadBuilderError> {
        self.inner.payload_attributes()
    }

    fn resolve_kind(
        &mut self,
        kind: PayloadKind,
    ) -> (Self::ResolvePayloadFuture, KeepPayloadJobAlive) {
        if let Some(payload) = self.best_payload.borrow().clone() {
            info!(
                payload_id = %payload.id(),
                block = payload.block().number,
                fees = %payload.fees(),
                "Resolving payload with rbuilder block"
            );
            return (
                futures::future::Either::Left(futures::future::ready(Ok(payload))),
                KeepPayloadJobAlive::No,
            );
        }
        info!("No rbuilder block available, resolving payload with fallback builder");
        let (fut, keep_alive) = self.inner.resolve_kind(kind);
        (futures::future::Either::Right(fut), keep_alive)
    }
}
