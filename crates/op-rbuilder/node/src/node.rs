//! op-rbuilder Node types config.
//!
//! Inherits Network, Executor, and Consensus Builders from the optimism node,
//! and overrides the Pool and Payload Builders.

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use rbuilder_bundle_pool_operations::BundlePoolOps;
use reth_basic_payload_builder::{
    BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig, BuildArguments, Cancelled,
    PayloadConfig, ResolveBestPayload,
};
use reth_evm::ConfigureEvm;
use reth_node_api::NodePrimitives;
use reth_node_builder::{
    components::{ComponentsBuilder, PayloadServiceBuilder, PoolBuilder},
    node::{FullNodeTypes, NodeTypes},
    BuilderContext, Node, NodeAdapter, NodeComponentsBuilder, NodeTypesWithEngine,
    PayloadBuilderConfig,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::OpEvmConfig;
use reth_optimism_node::{
    node::{
        OpConsensusBuilder, OpExecutorBuilder, OpNetworkBuilder, OpPoolBuilder, OpPrimitives,
        OptimismAddOns,
    },
    txpool::OpTransactionValidator,
    OpEngineTypes,
};
use reth_optimism_node::{OpBuiltPayload, OpPayloadBuilderAttributes};
use reth_payload_builder::{KeepPayloadJobAlive, PayloadJob};
use reth_payload_builder::{
    PayloadBuilderError, PayloadBuilderHandle, PayloadBuilderService, PayloadJobGenerator,
};
use reth_payload_primitives::{BuiltPayload, PayloadBuilderAttributes, PayloadKind};
use reth_primitives::SealedHeader;
use reth_primitives::{Header, TransactionSigned};
use reth_provider::BlockSource;
use reth_provider::CanonStateNotification;
use reth_provider::{BlockReader, CanonStateSubscriptions, DatabaseProviderFactory};
use reth_provider::{BlockReaderIdExt, StateProviderFactory};
use reth_revm::cached::CachedReads;
use reth_tasks::TaskSpawner;
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::TransactionPool;
use reth_transaction_pool::{
    blobstore::DiskFileBlobStore, CoinbaseTipOrdering, EthPooledTransaction,
    TransactionValidationTaskExecutor,
};
use reth_trie_db::MerklePatriciaTrie;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use std::{path::PathBuf, sync::Arc};
use transaction_pool_bundle_ext::{BundlePoolOperations, BundleSupportedPool};

use crate::args::OpRbuilderArgs;
use crate::builder::OpRbuilderPayloadBuilder;
use crate::cell::BlockCell;

/// Optimism primitive types.
#[derive(Debug)]
pub struct OpRbuilderPrimitives;

impl NodePrimitives for OpRbuilderPrimitives {
    type Block = reth_primitives::Block;
}

/// Type configuration for an Optimism rbuilder.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct OpRbuilderNode {
    /// Additional args
    pub args: OpRbuilderArgs,
}

impl OpRbuilderNode {
    /// Creates a new instance of the OP rbuilder node type.
    pub const fn new(args: OpRbuilderArgs) -> Self {
        Self { args }
    }

    /// Returns the components for the given [`OpRbuilderArgs`].
    pub fn components<Node>(
        args: OpRbuilderArgs,
    ) -> ComponentsBuilder<
        Node,
        OpPoolBuilder,
        OpRbuilderPayloadServiceBuilder,
        OpNetworkBuilder,
        OpExecutorBuilder,
        OpConsensusBuilder,
    >
    where
        Node: FullNodeTypes<
            Types: NodeTypesWithEngine<Engine = OpEngineTypes, ChainSpec = OpChainSpec>,
        >,
        <<Node as FullNodeTypes>::Provider as DatabaseProviderFactory>::Provider: BlockReader,
    {
        let OpRbuilderArgs {
            disable_txpool_gossip,
            compute_pending_block,
            discovery_v4,
            ..
        } = args;
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(OpPoolBuilder::default())
            .payload(OpRbuilderPayloadServiceBuilder::new(compute_pending_block))
            .network(OpNetworkBuilder {
                disable_txpool_gossip,
                disable_discovery_v4: !discovery_v4,
            })
            .executor(OpExecutorBuilder::default())
            .consensus(OpConsensusBuilder::default())
    }
}

impl<N> Node<N> for OpRbuilderNode
where
    N: FullNodeTypes<Types: NodeTypesWithEngine<Engine = OpEngineTypes, ChainSpec = OpChainSpec>>,
    <<N as FullNodeTypes>::Provider as DatabaseProviderFactory>::Provider: BlockReader,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        OpPoolBuilder,
        OpRbuilderPayloadServiceBuilder,
        OpNetworkBuilder,
        OpExecutorBuilder,
        OpConsensusBuilder,
    >;

    type AddOns = OptimismAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        let Self { args } = self;
        Self::components(args.clone())
    }

    fn add_ons(&self) -> Self::AddOns {
        OptimismAddOns::new(self.args.sequencer_http.clone())
    }
}

impl NodeTypes for OpRbuilderNode {
    type Primitives = OpPrimitives;
    type ChainSpec = OpChainSpec;
    type StateCommitment = MerklePatriciaTrie;
}

impl NodeTypesWithEngine for OpRbuilderNode {
    type Engine = OpEngineTypes;
}

/// An extended optimism transaction pool with bundle support.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct OpRbuilderPoolBuilder {
    rbuilder_config_path: PathBuf,
}

impl OpRbuilderPoolBuilder {
    /// Creates a new instance of the OP rbuilder pool builder.
    pub fn new(rbuilder_config_path: PathBuf) -> Self {
        Self {
            rbuilder_config_path,
        }
    }
}

pub type OpRbuilderTransactionPool<Client, S> = BundleSupportedPool<
    TransactionValidationTaskExecutor<OpTransactionValidator<Client, EthPooledTransaction>>,
    CoinbaseTipOrdering<EthPooledTransaction>,
    S,
    BundlePoolOps,
>;

impl<Node> PoolBuilder<Node> for OpRbuilderPoolBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = OpChainSpec>>,
    <<Node as FullNodeTypes>::Provider as DatabaseProviderFactory>::Provider: BlockReader,
{
    type Pool = OpRbuilderTransactionPool<Node::Provider, DiskFileBlobStore>;

    async fn build_pool(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Pool> {
        let data_dir = ctx.config().datadir();
        let blob_store = DiskFileBlobStore::open(data_dir.blobstore(), Default::default())?;

        let validator = TransactionValidationTaskExecutor::eth_builder(Arc::new(
            ctx.chain_spec().inner.clone(),
        ))
        .with_head_timestamp(ctx.head().timestamp)
        .kzg_settings(ctx.kzg_settings()?)
        .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
        .build_with_tasks(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            blob_store.clone(),
        )
        .map(|validator| {
            OpTransactionValidator::new(validator)
                // In --dev mode we can't require gas fees because we're unable to decode the L1
                // block info
                .require_l1_data_gas_fee(!ctx.config().dev.dev)
        });

        let bundle_ops = BundlePoolOps::new(ctx.provider().clone(), self.rbuilder_config_path)
            .await
            .expect("Failed to instantiate RbuilderBundlePoolOps");
        let transaction_pool = OpRbuilderTransactionPool::new(
            validator,
            CoinbaseTipOrdering::default(),
            blob_store,
            bundle_ops,
            ctx.pool_config(),
        );

        info!(target: "reth::cli", "Transaction pool initialized");
        let transactions_path = data_dir.txpool_transactions();

        // spawn txpool maintenance task
        {
            let pool = transaction_pool.clone();
            let chain_events = ctx.provider().canonical_state_stream();
            let client = ctx.provider().clone();
            let transactions_backup_config =
                reth_transaction_pool::maintain::LocalTransactionBackupConfig::with_local_txs_backup(transactions_path);

            ctx.task_executor()
                .spawn_critical_with_graceful_shutdown_signal(
                    "local transactions backup task",
                    |shutdown| {
                        reth_transaction_pool::maintain::backup_local_transactions_task(
                            shutdown,
                            pool.clone(),
                            transactions_backup_config,
                        )
                    },
                );

            // spawn the maintenance task
            ctx.task_executor().spawn_critical(
                "txpool maintenance task",
                reth_transaction_pool::maintain::maintain_transaction_pool_future(
                    client,
                    pool,
                    chain_events,
                    ctx.task_executor().clone(),
                    Default::default(),
                ),
            );
            debug!(target: "reth::cli", "Spawned txpool maintenance task");
        }

        Ok(transaction_pool)
    }
}

/// An OP rbuilder payload service builder.
#[derive(Debug, Default, Clone)]
pub struct OpRbuilderPayloadServiceBuilder {
    /// By default the pending block equals the latest block
    /// to save resources and not leak txs from the tx-pool,
    /// this flag enables computing of the pending block
    /// from the tx-pool instead.
    ///
    /// If `compute_pending_block` is not enabled, the payload builder
    /// will use the payload attributes from the latest block. Note
    /// that this flag is not yet functional.
    pub compute_pending_block: bool,
}

impl OpRbuilderPayloadServiceBuilder {
    /// Create a new instance with the given `compute_pending_block` flag.
    pub const fn new(compute_pending_block: bool) -> Self {
        Self {
            compute_pending_block,
        }
    }

    /// A helper method to initialize [`PayloadBuilderService`] with the given EVM config.
    pub fn spawn<Node, Evm, Pool>(
        self,
        evm_config: Evm,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<OpEngineTypes>>
    where
        Node: FullNodeTypes<
            Types: NodeTypesWithEngine<Engine = OpEngineTypes, ChainSpec = OpChainSpec>,
        >,
        <<Node as FullNodeTypes>::Provider as DatabaseProviderFactory>::Provider: BlockReader,
        Pool: TransactionPool + Unpin + 'static,
        Evm: ConfigureEvm<Header = Header>,
    {
        //let payload_builder = OpPayloadBuilder {};

        let payload_builder = OpRbuilderPayloadBuilder::new(evm_config);

        let payload_generator = FbPayloadJobGenerator {
            client: ctx.provider().clone(),
            pool,
            executor: ctx.task_executor().clone(),
            builder: payload_builder,
        };
        /*
        let payload_generator = BasicPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            pool,
            ctx.task_executor().clone(),
            payload_job_config,
            payload_builder,
        );
        */
        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical("payload builder service", Box::pin(payload_service));

        Ok(payload_builder)
    }
}

impl<Node, Pool> PayloadServiceBuilder<Node, Pool> for OpRbuilderPayloadServiceBuilder
where
    Node:
        FullNodeTypes<Types: NodeTypesWithEngine<Engine = OpEngineTypes, ChainSpec = OpChainSpec>>,
    <<Node as FullNodeTypes>::Provider as DatabaseProviderFactory>::Provider: BlockReader,
    Pool: TransactionPool + Unpin + 'static,
{
    async fn spawn_payload_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<OpEngineTypes>> {
        self.spawn(OpEvmConfig::new(ctx.chain_spec()), ctx, pool)
    }
}

struct FbPayloadJobGenerator<Client, Pool, Tasks, Builder> {
    /// The client that can interact with the chain.
    client: Client,
    /// The transaction pool to pull transactions from.
    pool: Pool,
    /// The task executor to spawn payload building tasks on.
    executor: Tasks,
    /// The type responsible for building payloads.
    ///
    /// See [`PayloadBuilder`]
    builder: Builder,
}

impl<Client, Pool, Tasks, Builder> PayloadJobGenerator
    for FbPayloadJobGenerator<Client, Pool, Tasks, Builder>
where
    Client: StateProviderFactory + BlockReaderIdExt + Clone + Unpin + 'static,
    Pool: TransactionPool + Unpin + 'static,
    Tasks: TaskSpawner + Clone + Unpin + 'static,
    Builder: PayloadBuilder<Pool, Client> + Unpin + 'static,
    <Builder as PayloadBuilder<Pool, Client>>::Attributes: Unpin + Clone,
    <Builder as PayloadBuilder<Pool, Client>>::BuiltPayload: Unpin + Clone,
{
    type Job = FbPayloadJob<Client, Pool, Tasks, Builder>;

    fn new_payload_job(
        &self,
        attributes: <Self::Job as PayloadJob>::PayloadAttributes,
    ) -> Result<Self::Job, PayloadBuilderError> {
        let parent_block = if attributes.parent().is_zero() {
            // use latest block if parent is zero: genesis block
            self.client
                .block_by_number_or_tag(BlockNumberOrTag::Latest)?
                .ok_or_else(|| PayloadBuilderError::MissingParentBlock(attributes.parent()))?
                .seal_slow()
        } else {
            let block = self
                .client
                .find_block_by_hash(attributes.parent(), BlockSource::Any)?
                .ok_or_else(|| PayloadBuilderError::MissingParentBlock(attributes.parent()))?;

            // we already know the hash, so we can seal it
            block.seal(attributes.parent())
        };

        let hash = parent_block.hash();
        let parent_header = parent_block.header();
        let header = SealedHeader::new(parent_header.clone(), hash);

        let config = PayloadConfig::new(Arc::new(header), Default::default(), attributes);

        //let until = self.job_deadline(config.attributes.timestamp());
        //let deadline = Box::pin(tokio::time::sleep_until(until));

        // let cached_reads = self.maybe_pre_cached(hash);

        let mut job = FbPayloadJob {
            config,
            client: self.client.clone(),
            pool: self.pool.clone(),
            executor: self.executor.clone(),
            builder: self.builder.clone(),
            cell: BlockCell::new(),
            best_payload: None,
        };

        // start the first job right away
        job.spawn_build_job();

        Ok(job)
    }
}

struct FbPayloadJob<Client, Pool, Tasks, Builder>
where
    Builder: PayloadBuilder<Pool, Client>,
{
    config: PayloadConfig<Builder::Attributes>,
    client: Client,
    pool: Pool,
    executor: Tasks,
    builder: Builder,
    cell: BlockCell<Builder::BuiltPayload>,
    best_payload: Option<Builder::BuiltPayload>,
}

impl<Client, Pool, Tasks, Builder> FbPayloadJob<Client, Pool, Tasks, Builder>
where
    Client: StateProviderFactory + Clone + Unpin + 'static,
    Pool: TransactionPool + Unpin + 'static,
    Tasks: TaskSpawner + Clone + 'static,
    Builder: PayloadBuilder<Pool, Client> + Unpin + 'static,
    <Builder as PayloadBuilder<Pool, Client>>::Attributes: Unpin + Clone,
    <Builder as PayloadBuilder<Pool, Client>>::BuiltPayload: Unpin + Clone,
{
    fn spawn_build_job(&mut self) {
        println!("SPAWN BUILD JOB");

        let builder = self.builder.clone();
        let cell = self.cell.clone(); // This is safe to clone since it's an Arc inside

        let client = self.client.clone();
        let pool = self.pool.clone();
        let payload_config = self.config.clone();
        let cancel = Cancelled::default(); // not used

        self.executor.spawn_blocking(Box::pin(async move {
            let args = BuildArguments {
                client,
                pool,
                cached_reads: CachedReads::default(),
                config: payload_config,
                cancel,
                best_payload: None,
            };

            builder.build(args, &cell); // Builder updates the cell directly
        }));
    }
}

impl<Client, Pool, Tasks, Builder> Future for FbPayloadJob<Client, Pool, Tasks, Builder>
where
    Client: StateProviderFactory + Clone + Unpin + Send + Sync + 'static,
    Pool: TransactionPool + Unpin + Send + Sync + 'static,
    Tasks: TaskSpawner + Clone + Send + Sync + 'static,
    Builder: PayloadBuilder<Pool, Client> + Unpin + Send + Sync + 'static,
    <Builder as PayloadBuilder<Pool, Client>>::Attributes: Unpin + Clone + Send + Sync,
    <Builder as PayloadBuilder<Pool, Client>>::BuiltPayload: Unpin + Clone + Send + Sync,
{
    type Output = Result<(), PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.cell.poll_updated(cx) {
            Poll::Ready(maybe_payload) => {
                this.best_payload = maybe_payload;
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<Client, Pool, Tasks, Builder> PayloadJob for FbPayloadJob<Client, Pool, Tasks, Builder>
where
    Client: StateProviderFactory + Clone + Unpin + 'static,
    Pool: TransactionPool + Unpin + 'static,
    Tasks: TaskSpawner + Clone + 'static,
    Builder: PayloadBuilder<Pool, Client> + Unpin + 'static,
    <Builder as PayloadBuilder<Pool, Client>>::Attributes: Unpin + Clone,
    <Builder as PayloadBuilder<Pool, Client>>::BuiltPayload: Unpin + Clone,
{
    type PayloadAttributes = Builder::Attributes;
    type ResolvePayloadFuture = ResolvePayload<Self::BuiltPayload>;
    type BuiltPayload = Builder::BuiltPayload;

    fn best_payload(&self) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        if let Some(best_payload) = &self.best_payload {
            return Ok(best_payload.clone());
        }
        Err(PayloadBuilderError::MissingPayload)
    }

    fn payload_attributes(&self) -> Result<Self::PayloadAttributes, PayloadBuilderError> {
        Ok(self.config.attributes.clone())
    }

    fn resolve_kind(
        &mut self,
        kind: PayloadKind,
    ) -> (Self::ResolvePayloadFuture, KeepPayloadJobAlive) {
        info!("resolve kind");

        let resolve_future = ResolvePayload::new(self.cell.clone());
        (resolve_future, KeepPayloadJobAlive::No)
    }
}

// A future that resolves when a payload becomes available in the BlockCell
pub struct ResolvePayload<T> {
    cell: BlockCell<T>,
}

impl<T> ResolvePayload<T> {
    pub fn new(cell: BlockCell<T>) -> Self {
        Self { cell }
    }
}

impl<T: Clone> Future for ResolvePayload<T> {
    type Output = Result<T, PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.get_mut().cell.poll_updated(cx) {
            Poll::Ready(Some(payload)) => Poll::Ready(Ok(payload)),
            Poll::Ready(None) => Poll::Ready(Err(PayloadBuilderError::MissingPayload)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Pre-filled [`CachedReads`] for a specific block.
///
/// This is extracted from the [`CanonStateNotification`] for the tip block.
#[derive(Debug, Clone)]
pub struct PrecachedState {
    /// The block for which the state is pre-cached.
    pub block: B256,
    /// Cached state for the block.
    pub cached: CachedReads,
}

pub trait PayloadBuilder<Pool, Client>: Send + Sync + Clone {
    type Attributes: PayloadBuilderAttributes;
    type BuiltPayload: BuiltPayload;

    fn build(
        &self,
        args: BuildArguments<Pool, Client, Self::Attributes, Self::BuiltPayload>,
        cell: &BlockCell<Self::BuiltPayload>,
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpPayloadBuilder {}

impl<Pool, Client> PayloadBuilder<Pool, Client> for OpPayloadBuilder
where
    Pool: TransactionPool + Unpin + 'static,
    Client: StateProviderFactory + Clone + Unpin + 'static,
{
    type Attributes = OpPayloadBuilderAttributes;
    type BuiltPayload = OpBuiltPayload;

    fn build(
        &self,
        args: BuildArguments<Pool, Client, Self::Attributes, Self::BuiltPayload>,
        cell: &BlockCell<Self::BuiltPayload>,
    ) {
        // Implementation here
    }
}
