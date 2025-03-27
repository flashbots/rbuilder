use crate::{
    executor::OpRbuilderEvmConfig,
    generator::{BlockPayloadJobGenerator, BuildArguments, PayloadBuilder, PayloadBuilderBuilder},
    metrics::OpRBuilderMetrics,
    primitives::{reth::ExecutionInfo, supervisor::SupervisorValidator},
    tx_signer::OpSigner,
};
use alloy_consensus::transaction::Recovered;
use alloy_consensus::{Header, Transaction, Typed2718};
use alloy_primitives::private::alloy_rlp::Encodable;
use alloy_primitives::{Address, Bytes, TxHash, TxKind, B256, U256};
use alloy_rpc_types_engine::PayloadId;
use alloy_rpc_types_eth::TransactionRequest;
use jsonrpsee::http_client::HttpClientBuilder;
use kona_interop::{ExecutingDescriptor, SafetyLevel};
use kona_rpc::{InteropTxValidator, InteropTxValidatorError};
use reth::builder::components::PayloadServiceBuilder;
use reth::core::primitives::InMemorySize;
use reth::payload::PayloadBuilderHandle;
use reth_basic_payload_builder::{
    is_better_payload, BasicPayloadJobGeneratorConfig, BuildOutcome, BuildOutcomeKind,
    PayloadConfig,
};
use reth_chain_state::{ExecutedBlock, ExecutedBlockWithTrieUpdates};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_evm::{
    block::{BlockExecutionError, BlockValidationError},
    execute::{BlockBuilder, BlockBuilderOutcome},
    ConfigureEvm, Database, Evm,
};
use reth_execution_types::ExecutionOutcome;
use reth_node_api::{NodePrimitives, PrimitivesTy, TxTy};
use reth_node_builder::{
    node::{FullNodeTypes, NodeTypesWithEngine},
    BuilderContext,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::OpNextBlockEnvAttributes;
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::{txpool::OpPooledTx, OpEngineTypes};
use reth_optimism_payload_builder::config::{OpBuilderConfig, OpDAConfig};
use reth_optimism_payload_builder::OpPayloadPrimitives;
use reth_optimism_payload_builder::{
    error::OpPayloadBuilderError,
    payload::{OpBuiltPayload, OpPayloadBuilderAttributes},
};
use reth_optimism_primitives::{OpPrimitives, OpTransactionSigned};
use reth_payload_builder::PayloadBuilderService;
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_payload_util::BestPayloadTransactions;
use reth_payload_util::PayloadTransactions;
use reth_primitives::{BlockBody, SealedHeader};
use reth_primitives_traits::{SignedTransaction, TxTy as PrimitivesTxTy};
use reth_provider::{CanonStateSubscriptions, StateProvider};
use reth_provider::{ProviderError, StateProviderFactory};
use reth_revm::{cancelled::CancelOnDrop, database::StateProviderDatabase, State};
use reth_transaction_pool::BestTransactionsAttributes;
use reth_transaction_pool::PoolTransaction;
use reth_transaction_pool::TransactionPool;
use revm::context::{Block, BlockEnv};
use std::{sync::Arc, time::Instant};
use tracing::{error, info, trace, warn};
use url::Url;

#[derive(Debug, Clone, Default)]
pub struct CustomOpPayloadBuilder<Txs = ()> {
    /// The type responsible for yielding the best transactions for the payload if mempool
    /// transactions are allowed.
    pub best_transactions: Txs,
    /// This data availability configuration specifies constraints for the payload builder
    /// when assembling payloads
    pub da_config: OpDAConfig,
    /// The builder's signer key to use for an end of block tx
    pub builder_signer: Option<OpSigner>,
    /// The URL of the supervisor for validation
    pub supervisor_url: Option<Url>,
    /// The safety level for the supervisor
    pub supervisor_safety_level: Option<String>,
}

impl CustomOpPayloadBuilder {
    pub fn new(
        builder_signer: Option<OpSigner>,
        _flashblocks_ws_url: String,
        _chain_block_time: u64,
        _flashblock_block_time: u64,
        supervisor_url: Option<Url>,
        supervisor_safety_level: Option<String>,
    ) -> Self {
        Self {
            best_transactions: (),
            da_config: OpDAConfig::default(),
            builder_signer,
            supervisor_url,
            supervisor_safety_level,
        }
    }

    /// Configure the data availability configuration for the OP payload builder.
    pub fn with_da_config(mut self, da_config: OpDAConfig) -> Self {
        self.da_config = da_config;
        self
    }
}

impl<Txs> CustomOpPayloadBuilder<Txs> {
    /// A helper method to initialize [`OpPayloadBuilderVanilla`] with the
    /// given EVM config.
    pub fn build<Node, Evm, Pool>(
        self,
        evm_config: Evm,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<OpPayloadBuilderVanilla<Pool, Node::Provider, Evm, Txs>>
    where
        Node: FullNodeTypes<
            Types: NodeTypesWithEngine<
                Engine = OpEngineTypes,
                ChainSpec = OpChainSpec,
                Primitives = OpPrimitives,
            >,
        >,
        Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
            + Unpin
            + 'static,
        Evm: ConfigureEvm<Primitives = PrimitivesTy<Node::Types>>,
        Txs: OpPayloadTransactions<Pool::Transaction>,
    {
        let payload_builder = OpPayloadBuilderVanilla::with_builder_config(
            evm_config,
            self.builder_signer,
            pool,
            ctx.provider().clone(),
            self.supervisor_url.clone(),
            self.supervisor_safety_level.clone(),
            OpBuilderConfig {
                da_config: self.da_config.clone(),
            },
        )
        .with_transactions(self.best_transactions.clone());
        Ok(payload_builder)
    }
}

impl<Node, Pool, Txs> PayloadBuilderBuilder<Node, Pool> for CustomOpPayloadBuilder<Txs>
where
    Node: FullNodeTypes<
        Types: NodeTypesWithEngine<
            Engine = OpEngineTypes,
            ChainSpec = OpChainSpec,
            Primitives = OpPrimitives,
        >,
    >,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
    Txs: OpPayloadTransactions<Pool::Transaction>,
    <Pool as TransactionPool>::Transaction: OpPooledTx,
{
    type PayloadBuilder = OpPayloadBuilderVanilla<Pool, Node::Provider, OpRbuilderEvmConfig, Txs>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<Self::PayloadBuilder> {
        self.build(OpRbuilderEvmConfig::optimism(ctx.chain_spec()), ctx, pool)
    }
}

impl<Node, Pool> PayloadServiceBuilder<Node, Pool> for CustomOpPayloadBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypesWithEngine<
            Engine = OpEngineTypes,
            ChainSpec = OpChainSpec,
            Primitives = OpPrimitives,
        >,
    >,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
    <Pool as TransactionPool>::Transaction: OpPooledTx,
{
    async fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypesWithEngine>::Engine>> {
        tracing::info!("Spawning a vanilla payload builder service");
        let payload_builder = self.build_payload_builder(ctx, pool).await?;
        let payload_job_config = BasicPayloadJobGeneratorConfig::default();

        let payload_generator = BlockPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            payload_job_config,
            payload_builder,
        );

        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical("custom payload builder service", Box::pin(payload_service));

        tracing::info!("Vanilla payload builder service started");

        Ok(payload_builder)
    }
}

/// Optimism's payload builder
#[derive(Debug, Clone)]
pub struct OpPayloadBuilderVanilla<Pool, Client, Evm: ConfigureEvm, Txs = ()> {
    /// The type responsible for creating the evm.
    pub evm_config: Evm,
    /// The builder's signer key to use for an end of block tx
    pub builder_signer: Option<OpSigner>,
    /// The transaction pool
    pub pool: Pool,
    /// Node client
    pub client: Client,
    /// Settings for the builder, e.g. DA settings.
    pub config: OpBuilderConfig,
    /// The type responsible for yielding the best transactions for the payload if mempool
    /// transactions are allowed.
    pub best_transactions: Txs,
    /// The metrics for the builder
    pub metrics: OpRBuilderMetrics,
    /// Client to execute supervisor validation
    pub supervisor_client: Option<SupervisorValidator>,
    /// Level to use in supervisor validation
    pub supervisor_safety_level: SafetyLevel,
}

impl<Pool, Client, Evm> OpPayloadBuilderVanilla<Pool, Client, Evm>
where
    Evm: ConfigureEvm<Primitives = OpPrimitives>,
{
    // TODO: we will move supervisor_url and supervisor_safety_level into OpBuilderConfig to reduce
    // number of args
    #[allow(clippy::too_many_arguments)]
    pub fn with_builder_config(
        evm_config: Evm,
        builder_signer: Option<OpSigner>,
        pool: Pool,
        client: Client,
        supervisor_url: Option<Url>,
        supervisor_safety_level: Option<String>,
        config: OpBuilderConfig,
    ) -> Self {
        // TODO: we should make this client required if interop hardfork enabled, add this after spec rebase
        let supervisor_client = supervisor_url.map(|url| {
            SupervisorValidator::new(
                HttpClientBuilder::default()
                    .build(url)
                    .expect("building supervisor http client"),
            )
        });
        let supervisor_safety_level = supervisor_safety_level
            .map(|level| {
                serde_json::from_str(level.as_str()).expect("parsing supervisor_safety_level")
            })
            .unwrap_or(SafetyLevel::CrossUnsafe);
        Self {
            pool,
            client,
            config,
            evm_config,
            best_transactions: (),
            metrics: Default::default(),
            builder_signer,
            supervisor_client,
            supervisor_safety_level,
        }
    }
}

impl<Pool, Client, Evm, Txs> OpPayloadBuilderVanilla<Pool, Client, Evm, Txs>
where
    Evm: ConfigureEvm<Primitives = OpPrimitives>,
{
    /// Configures the type responsible for yielding the transactions that should be included in the
    /// payload.
    pub fn with_transactions<T>(
        self,
        best_transactions: T,
    ) -> OpPayloadBuilderVanilla<Pool, Client, Evm, T> {
        let Self {
            pool,
            client,
            evm_config,
            config,
            builder_signer,
            metrics,
            supervisor_client,
            supervisor_safety_level,
            ..
        } = self;
        OpPayloadBuilderVanilla {
            pool,
            client,
            evm_config,
            best_transactions,
            config,
            builder_signer,
            metrics,
            supervisor_client,
            supervisor_safety_level,
        }
    }
}

impl<Evm, Pool, Client, N, Txs> PayloadBuilder for OpPayloadBuilderVanilla<Pool, Client, Evm, Txs>
where
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthChainSpec + OpHardforks> + Clone,
    N: OpPayloadPrimitives,
    Pool: TransactionPool<Transaction: OpPooledTx<Consensus = N::SignedTx>>,
    Evm: ConfigureEvm<Primitives = N, NextBlockEnvCtx = OpNextBlockEnvAttributes>,
    Evm::Primitives: OpPayloadPrimitives<_TX = OpTransactionSigned>,
    Txs: OpPayloadTransactions<Pool::Transaction>,
{
    type Attributes = OpPayloadBuilderAttributes<N::SignedTx>;
    type BuiltPayload = OpBuiltPayload<N>;

    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        let pool = self.pool.clone();
        let start = Instant::now();
        self.build_payload(
            args,
            |attrs| {
                self.best_transactions
                    .best_transactions(pool.clone(), attrs)
            },
            |hashes| self.best_transactions.remove_invalid(pool.clone(), hashes),
        )
        .inspect(|_| {
            self.metrics
                .total_block_built_duration
                .record(start.elapsed());
        })
    }
}

impl<Pool, Client, Evm, N, T> OpPayloadBuilderVanilla<Pool, Client, Evm, T>
where
    Pool: TransactionPool<Transaction: OpPooledTx<Consensus = N::SignedTx>>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthChainSpec + OpHardforks>,
    N: OpPayloadPrimitives,
    Evm: ConfigureEvm<Primitives = N, NextBlockEnvCtx = OpNextBlockEnvAttributes>,
    Evm::Primitives: OpPayloadPrimitives<
        BlockHeader = Header,
        BlockBody = BlockBody<<Evm::Primitives as NodePrimitives>::SignedTx>,
        _TX = OpTransactionSigned,
    >,
{
    /// Constructs an Optimism payload from the transactions sent via the
    /// Payload attributes by the sequencer. If the `no_tx_pool` argument is passed in
    /// the transaction pool will be ignored and the only transactions
    /// included in the payload will be those sent through the attributes.
    ///
    /// Given build arguments including an Optimism client, transaction pool,
    /// and configuration, this function creates a transaction payload. Returns
    /// a result indicating success with the payload or an error in case of failure.
    fn build_payload<'a, Txs>(
        &self,
        args: BuildArguments<OpPayloadBuilderAttributes<N::SignedTx>, OpBuiltPayload<N>>,
        best: impl FnOnce(BestTransactionsAttributes) -> Txs + Send + Sync + 'a,
        remove_reverted: impl FnOnce(Vec<TxHash>) + 'a,
    ) -> Result<BuildOutcome<OpBuiltPayload<N>>, PayloadBuilderError>
    where
        Txs: PayloadTransactions<Transaction: PoolTransaction<Consensus = OpTransactionSigned>>,
    {
        let BuildArguments {
            mut cached_reads,
            config,
            cancel,
            best_payload,
        } = args;

        let chain_spec = self.client.chain_spec();

        let ctx = OpPayloadBuilderCtx {
            evm_config: self.evm_config.clone(),
            da_config: self.config.da_config.clone(),
            chain_spec,
            config,
            cancel,
            best_payload,
            builder_signer: self.builder_signer,
            metrics: Default::default(),
            supervisor_client: self.supervisor_client.clone(),
            supervisor_safety_level: self.supervisor_safety_level,
        };

        let builder = OpBuilder::new(best, remove_reverted);

        let state_provider = self.client.state_by_block_hash(ctx.parent().hash())?;
        let state = StateProviderDatabase::new(&state_provider);

        if ctx.attributes().no_tx_pool {
            builder.build(state, &state_provider, ctx)
        } else {
            // sequencer mode we can reuse cachedreads from previous runs
            builder.build(cached_reads.as_db_mut(state), &state_provider, ctx)
        }
        .map(|out| out.with_cached_reads(cached_reads))
    }
}

/// The type that builds the payload.
///
/// Payload building for optimism is composed of several steps.
/// The first steps are mandatory and defined by the protocol.
///
/// 1. first all System calls are applied.
/// 2. After canyon the forced deployed `create2deployer` must be loaded
/// 3. all sequencer transactions are executed (part of the payload attributes)
///
/// Depending on whether the node acts as a sequencer and is allowed to include additional
/// transactions (`no_tx_pool == false`):
/// 4. include additional transactions
///
/// And finally
/// 5. build the block: compute all roots (txs, state)
#[derive(derive_more::Debug)]
pub struct OpBuilder<'a, Txs> {
    /// Yields the best transaction to include if transactions from the mempool are allowed.
    best: Box<dyn FnOnce(BestTransactionsAttributes) -> Txs + 'a>,
    /// Removes reverted transactions from the tx pool
    #[debug(skip)]
    remove_invalid: Box<dyn FnOnce(Vec<TxHash>) + 'a>,
}

impl<'a, Txs> OpBuilder<'a, Txs> {
    fn new(
        best: impl FnOnce(BestTransactionsAttributes) -> Txs + Send + Sync + 'a,
        remove_reverted: impl FnOnce(Vec<TxHash>) + 'a,
    ) -> Self {
        Self {
            best: Box::new(best),
            remove_invalid: Box::new(remove_reverted),
        }
    }
}

impl<Txs> OpBuilder<'_, Txs> {
    /// Builds the payload on top of the state.
    pub fn build<EvmConfig, ChainSpec, N>(
        self,
        db: impl Database<Error = ProviderError>,
        state_provider: impl StateProvider,
        ctx: OpPayloadBuilderCtx<EvmConfig, ChainSpec>,
    ) -> Result<BuildOutcomeKind<OpBuiltPayload<N>>, PayloadBuilderError>
    where
        EvmConfig: ConfigureEvm<Primitives = N, NextBlockEnvCtx = OpNextBlockEnvAttributes>,
        ChainSpec: EthChainSpec + OpHardforks,
        N: OpPayloadPrimitives,
        EvmConfig::Primitives: OpPayloadPrimitives<_TX = OpTransactionSigned>,
        Txs: PayloadTransactions<Transaction: PoolTransaction<Consensus = N::SignedTx>>,
    {
        let Self {
            best,
            remove_invalid,
        } = self;
        info!(target: "payload_builder", id=%ctx.payload_id(), parent_header = ?ctx.parent().hash(), parent_number = ctx.parent().number, "building new payload");

        let mut db = State::builder()
            .with_database(db)
            .with_bundle_update()
            .build();

        let mut builder = ctx.block_builder(&mut db)?;

        // 1. apply pre-execution changes
        builder.apply_pre_execution_changes().map_err(|err| {
            warn!(target: "payload_builder", %err, "failed to apply pre-execution changes");
            PayloadBuilderError::Internal(err.into())
        })?;

        let sequencer_tx_start_time = Instant::now();

        // 2. execute sequencer transactions
        let mut info = ctx.execute_sequencer_transactions(&mut builder)?;

        ctx.metrics
            .sequencer_tx_duration
            .record(sequencer_tx_start_time.elapsed());

        // reserve gas for builder tx
        let message = format!("Block Number: {}", builder.evm_mut().block().number)
            .as_bytes()
            .to_vec();
        let builder_tx_gas = ctx
            .builder_signer()
            .map_or(0, |_| estimate_gas_for_builder_tx(message.clone()));
        let block_gas_limit = builder.evm_mut().block().gas_limit - builder_tx_gas;
        // Save some space in the block_da_limit for builder tx
        let builder_tx_da_size = ctx
            .estimate_builder_tx_da_size(
                &state_provider,
                builder.evm_mut().block().basefee,
                builder_tx_gas,
                message.clone(),
            )
            .unwrap_or(0);
        let block_da_limit = ctx
            .da_config
            .max_da_block_size()
            .map(|da_size| da_size - builder_tx_da_size as u64);
        // Check that it's possible to create builder tx, considering max_da_tx_size, otherwise panic
        if let Some(tx_da_limit) = ctx.da_config.max_da_tx_size() {
            // Panic indicate max_da_tx_size misconfiguration
            assert!(
                tx_da_limit >= builder_tx_da_size as u64,
                "The configured da_config.max_da_tx_size is too small to accommodate builder tx."
            );
        }

        // 3. if mem pool transactions are requested we execute them
        if !ctx.attributes().no_tx_pool {
            let best_txs_start_time = Instant::now();
            let best_txs = best(ctx.best_transaction_attributes(builder.evm_mut().block()));
            ctx.metrics
                .transaction_pool_fetch_duration
                .record(best_txs_start_time.elapsed());

            if ctx
                .execute_best_transactions(
                    &mut info,
                    &mut builder,
                    best_txs,
                    block_gas_limit,
                    block_da_limit,
                )?
                .is_some()
            {
                return Ok(BuildOutcomeKind::Cancelled);
            }

            // check if the new payload is even more valuable
            if !ctx.is_better_payload(info.total_fees) {
                // can skip building the block
                return Ok(BuildOutcomeKind::Aborted {
                    fees: info.total_fees,
                });
            }
        }

        // Add builder tx to the block
        ctx.add_builder_tx(
            &mut info,
            &mut builder,
            &state_provider,
            builder_tx_gas,
            message,
        );

        let state_merge_start_time = Instant::now();
        let BlockBuilderOutcome {
            execution_result,
            hashed_state,
            trie_updates,
            block,
        } = builder.finish(state_provider)?;
        ctx.metrics
            .state_transition_merge_duration
            .record(state_merge_start_time.elapsed());

        ctx.metrics
            .payload_num_tx
            .record(execution_result.receipts.len() as f64);

        let sealed_block = Arc::new(block.sealed_block().clone());
        info!(target: "payload_builder", id=%ctx.attributes().payload_id(), sealed_block_header = ?sealed_block.header(), "sealed built block");

        let execution_outcome = ExecutionOutcome::new(
            db.take_bundle(),
            vec![execution_result.receipts],
            block.number,
            Vec::new(),
        );

        // create the executed block data
        let executed: ExecutedBlockWithTrieUpdates<N> = ExecutedBlockWithTrieUpdates {
            block: ExecutedBlock {
                recovered_block: Arc::new(block),
                execution_output: Arc::new(execution_outcome),
                hashed_state: Arc::new(hashed_state),
            },
            trie: Arc::new(trie_updates),
        };

        let no_tx_pool = ctx.attributes().no_tx_pool;

        let payload = OpBuiltPayload::new(
            ctx.payload_id(),
            sealed_block,
            info.total_fees,
            Some(executed),
        );

        remove_invalid(info.invalid_tx_hashes.iter().copied().collect());

        if no_tx_pool {
            // if `no_tx_pool` is set only transactions from the payload attributes will be included
            // in the payload. In other words, the payload is deterministic and we can
            // freeze it once we've successfully built it.
            Ok(BuildOutcomeKind::Freeze(payload))
        } else {
            ctx.metrics.block_built_success.increment(1);
            ctx.metrics
                .payload_byte_size
                .record(payload.block().size() as f64);
            Ok(BuildOutcomeKind::Better { payload })
        }
    }
}

/// A type that returns a the [`PayloadTransactions`] that should be included in the pool.
pub trait OpPayloadTransactions<Transaction>: Clone + Send + Sync + Unpin + 'static {
    /// Returns an iterator that yields the transaction in the order they should get included in the
    /// new payload.
    fn best_transactions<Pool: TransactionPool<Transaction = Transaction>>(
        &self,
        pool: Pool,
        attr: BestTransactionsAttributes,
    ) -> impl PayloadTransactions<Transaction = Transaction>;

    /// Removes invalid transactions from the tx pool
    fn remove_invalid<Pool: TransactionPool<Transaction = Transaction>>(
        &self,
        pool: Pool,
        hashes: Vec<TxHash>,
    );
}

impl<T: PoolTransaction> OpPayloadTransactions<T> for () {
    fn best_transactions<Pool: TransactionPool<Transaction = T>>(
        &self,
        pool: Pool,
        attr: BestTransactionsAttributes,
    ) -> impl PayloadTransactions<Transaction = T> {
        BestPayloadTransactions::new(pool.best_transactions_with_attributes(attr))
    }

    fn remove_invalid<Pool: TransactionPool<Transaction = T>>(
        &self,
        pool: Pool,
        hashes: Vec<TxHash>,
    ) {
        pool.remove_transactions(hashes);
    }
}

/// Container type that holds all necessities to build a new payload.
// #[derive(derive_more::Debug)]
pub struct OpPayloadBuilderCtx<Evm: ConfigureEvm, ChainSpec> {
    /// The type that knows how to perform system calls and configure the evm.
    pub evm_config: Evm,
    /// The DA config for the payload builder
    pub da_config: OpDAConfig,
    /// The chainspec
    pub chain_spec: Arc<ChainSpec>,
    /// How to build the payload.
    pub config: PayloadConfig<OpPayloadBuilderAttributes<PrimitivesTxTy<Evm::Primitives>>>,
    /// Marker to check whether the job has been cancelled.
    pub cancel: CancelOnDrop,
    /// The currently best payload.
    pub best_payload: Option<OpBuiltPayload<Evm::Primitives>>,
    /// The builder signer
    pub builder_signer: Option<OpSigner>,
    /// The metrics for the builder
    pub metrics: OpRBuilderMetrics,
    /// Client to execute supervisor validation
    pub supervisor_client: Option<SupervisorValidator>,
    /// Level to use in supervisor validation
    pub supervisor_safety_level: SafetyLevel,
}

impl<Evm, ChainSpec> OpPayloadBuilderCtx<Evm, ChainSpec>
where
    Evm: ConfigureEvm<Primitives: OpPayloadPrimitives, NextBlockEnvCtx = OpNextBlockEnvAttributes>,
    Evm::Primitives: OpPayloadPrimitives<_TX = OpTransactionSigned>,
    ChainSpec: EthChainSpec + OpHardforks,
{
    /// Returns the parent block the payload will be build on.
    pub fn parent(&self) -> &SealedHeader {
        &self.config.parent_header
    }

    /// Returns the builder attributes.
    pub const fn attributes(&self) -> &OpPayloadBuilderAttributes<PrimitivesTxTy<Evm::Primitives>> {
        &self.config.attributes
    }

    /// Returns the extra data for the block.
    ///
    /// After holocene this extracts the extradata from the paylpad
    pub fn extra_data(&self) -> Result<Bytes, PayloadBuilderError> {
        if self.is_holocene_active() {
            self.attributes()
                .get_holocene_extra_data(
                    self.chain_spec.base_fee_params_at_timestamp(
                        self.attributes().payload_attributes.timestamp,
                    ),
                )
                .map_err(PayloadBuilderError::other)
        } else {
            Ok(Default::default())
        }
    }

    /// Returns the current fee settings for transactions from the mempool
    pub fn best_transaction_attributes(&self, block_env: &BlockEnv) -> BestTransactionsAttributes {
        BestTransactionsAttributes::new(
            block_env.basefee,
            block_env.blob_gasprice().map(|p| p as u64),
        )
    }

    /// Returns the unique id for this payload job.
    pub fn payload_id(&self) -> PayloadId {
        self.attributes().payload_id()
    }

    /// Returns true if holocene is active for the payload.
    pub fn is_holocene_active(&self) -> bool {
        self.chain_spec
            .is_holocene_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns the chain id
    pub fn chain_id(&self) -> u64 {
        self.chain_spec.chain_id()
    }

    /// Returns true if the fees are higher than the previous payload.
    pub fn is_better_payload(&self, total_fees: U256) -> bool {
        is_better_payload(self.best_payload.as_ref(), total_fees)
    }

    /// Returns the builder signer
    pub fn builder_signer(&self) -> Option<OpSigner> {
        self.builder_signer
    }

    /// Prepares a [`BlockBuilder`] for the next block.
    pub fn block_builder<'a, DB: Database>(
        &'a self,
        db: &'a mut State<DB>,
    ) -> Result<impl BlockBuilder<Primitives = Evm::Primitives> + 'a, PayloadBuilderError> {
        self.evm_config
            .builder_for_next_block(
                db,
                self.parent(),
                OpNextBlockEnvAttributes {
                    timestamp: self.attributes().timestamp(),
                    suggested_fee_recipient: self.attributes().suggested_fee_recipient(),
                    prev_randao: self.attributes().prev_randao(),
                    gas_limit: self
                        .attributes()
                        .gas_limit
                        .unwrap_or(self.parent().gas_limit),
                    parent_beacon_block_root: self.attributes().parent_beacon_block_root(),
                    extra_data: self.extra_data()?,
                },
            )
            .map_err(PayloadBuilderError::other)
    }
}

impl<Evm, ChainSpec> OpPayloadBuilderCtx<Evm, ChainSpec>
where
    Evm: ConfigureEvm<Primitives: OpPayloadPrimitives, NextBlockEnvCtx = OpNextBlockEnvAttributes>,
    Evm::Primitives: OpPayloadPrimitives<
        BlockHeader = Header,
        BlockBody = BlockBody<<Evm::Primitives as NodePrimitives>::SignedTx>,
        _TX = OpTransactionSigned,
    >,
    ChainSpec: EthChainSpec + OpHardforks,
{
    /// Executes all sequencer transactions that are included in the payload attributes.
    pub fn execute_sequencer_transactions(
        &self,
        builder: &mut impl BlockBuilder<Primitives = Evm::Primitives>,
    ) -> Result<ExecutionInfo, PayloadBuilderError> {
        let mut info = ExecutionInfo::new();

        for sequencer_tx in &self.attributes().transactions {
            // A sequencer's block should never contain blob transactions.
            if sequencer_tx.value().is_eip4844() {
                return Err(PayloadBuilderError::other(
                    OpPayloadBuilderError::BlobTransactionRejected,
                ));
            }

            // Check transactions against supervisor if it's cross chain
            if let (false, _) = self.is_cross_tx_valid(
                sequencer_tx.value(),
                self.supervisor_client.as_ref(),
                self.supervisor_safety_level,
                self.config.attributes.timestamp(),
                &self.metrics,
            ) {
                // We skip this transaction because it's not possible to verify it's validity
                continue;
            }

            // Convert the transaction to a [RecoveredTx]. This is
            // purely for the purposes of utilizing the `evm_config.tx_env`` function.
            // Deposit transactions do not have signatures, so if the tx is a deposit, this
            // will just pull in its `from` address.
            let sequencer_tx = sequencer_tx
                .value()
                .try_clone_into_recovered()
                .map_err(|_| {
                    PayloadBuilderError::other(OpPayloadBuilderError::TransactionEcRecoverFailed)
                })?;

            let gas_used = match builder.execute_transaction(sequencer_tx.clone()) {
                Ok(gas_used) => gas_used,
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) => {
                    trace!(target: "payload_builder", %error, ?sequencer_tx, "Error in sequencer transaction, skipping.");
                    continue;
                }
                Err(err) => {
                    // this is an error that we should treat as fatal for this attempt
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            // add gas used by the transaction to cumulative gas used, before creating the receipt
            info.cumulative_gas_used += gas_used;
        }

        Ok(info)
    }

    /// Executes the given best transactions and updates the execution info.
    ///
    /// Returns `Ok(Some(())` if the job was cancelled.
    pub fn execute_best_transactions(
        &self,
        info: &mut ExecutionInfo,
        builder: &mut impl BlockBuilder<Primitives = Evm::Primitives>,
        mut best_txs: impl PayloadTransactions<
            Transaction: PoolTransaction<Consensus = PrimitivesTxTy<Evm::Primitives>>,
        >,
        block_gas_limit: u64,
        block_da_limit: Option<u64>,
    ) -> Result<Option<()>, PayloadBuilderError> {
        let execute_txs_start_time = Instant::now();
        let mut num_txs_considered = 0;
        let mut num_txs_simulated = 0;
        let mut num_txs_simulated_success = 0;
        let mut num_txs_simulated_fail = 0;

        let tx_da_limit = self.da_config.max_da_tx_size();
        let base_fee = builder.evm_mut().block().basefee;

        while let Some(tx) = best_txs.next(()) {
            let tx = tx.into_consensus();
            num_txs_considered += 1;
            // ensure we still have capacity for this transaction
            if info.is_tx_over_limits(tx.inner(), block_gas_limit, tx_da_limit, block_da_limit) {
                // we can't fit this transaction into the block, so we need to mark it as
                // invalid which also removes all dependent transaction from
                // the iterator before we can continue
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            // A sequencer's block should never contain blob or deposit transactions from the pool.
            if tx.is_eip4844() || tx.is_deposit() {
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            // Check transactions against supervisor if it's cross chain
            if let (false, is_recoverable) = self.is_cross_tx_valid(
                tx.inner(),
                self.supervisor_client.as_ref(),
                self.supervisor_safety_level,
                self.config.attributes.timestamp(),
                &self.metrics,
            ) {
                // We mark the tx invalid to ensure that it won't clog out pipeline
                // in case there is bug in supervisor.
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                if !is_recoverable {
                    // For some subset of errors we remove transaction from txpool
                    info.invalid_tx_hashes.insert(*tx.tx_hash());
                }
                continue;
            }

            // check if the job was cancelled, if so we can exit early
            if self.cancel.is_cancelled() {
                return Ok(Some(()));
            }

            let tx_simulation_start_time = Instant::now();

            let gas_used = match builder.execute_transaction_with_result_closure(
                tx.clone(),
                |result| {
                    if result.is_success() {
                        num_txs_simulated_success += 1;
                    } else {
                        num_txs_simulated_fail += 1;
                        trace!(target: "payload_builder", ?tx, "skipping reverted transaction");
                        best_txs.mark_invalid(tx.signer(), tx.nonce());
                        info.invalid_tx_hashes.insert(*tx.tx_hash());
                    }
                },
            ) {
                Ok(gas_used) => gas_used,
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) => {
                    if error.is_nonce_too_low() {
                        // if the nonce is too low, we can skip this transaction
                        trace!(target: "payload_builder", %error, ?tx, "skipping nonce too low transaction");
                    } else {
                        // if the transaction is invalid, we can skip it and all of its
                        // descendants
                        trace!(target: "payload_builder", %error, ?tx, "skipping invalid transaction and its descendants");
                        best_txs.mark_invalid(tx.signer(), tx.nonce());
                    }
                    continue;
                }
                Err(err) => {
                    // this is an error that we should treat as fatal for this attempt
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            // add gas used by the transaction to cumulative gas used, before creating the
            // receipt
            info.cumulative_gas_used += gas_used;
            info.cumulative_da_bytes_used += tx.length() as u64;

            // update add to total fees
            let miner_fee = tx
                .effective_tip_per_gas(base_fee)
                .expect("fee is always valid; execution succeeded");
            info.total_fees += U256::from(miner_fee) * U256::from(gas_used);

            self.metrics
                .tx_simulation_duration
                .record(tx_simulation_start_time.elapsed());
            self.metrics.tx_byte_size.record(tx.inner().size() as f64);
            num_txs_simulated += 1;
        }

        self.metrics
            .payload_tx_simulation_duration
            .record(execute_txs_start_time.elapsed());
        self.metrics
            .payload_num_tx_considered
            .record(num_txs_considered as f64);
        self.metrics
            .payload_num_tx_simulated
            .record(num_txs_simulated as f64);
        self.metrics
            .payload_num_tx_simulated_success
            .record(num_txs_simulated_success as f64);
        self.metrics
            .payload_num_tx_simulated_fail
            .record(num_txs_simulated_fail as f64);

        Ok(None)
    }

    /// Creates signed builder tx to Address::ZERO and specified message as input
    pub fn signed_builder_tx(
        &self,
        db: &impl StateProvider,
        builder_tx_gas: u64,
        message: Vec<u8>,
        signer: OpSigner,
        base_fee: u64,
        chain_id: u64,
    ) -> Result<Recovered<PrimitivesTxTy<Evm::Primitives>>, PayloadBuilderError> {
        // Create message with block number for the builder to sign
        let nonce = db
            .account_nonce(&signer.address)
            .map_err(|_| {
                PayloadBuilderError::other(OpPayloadBuilderError::AccountLoadFailed(signer.address))
            })?
            .unwrap_or_default();

        let request = TransactionRequest {
            chain_id: Some(chain_id),
            nonce: Some(nonce),
            gas: Some(builder_tx_gas),
            max_fee_per_gas: Some(base_fee.into()),
            max_priority_fee_per_gas: Some(0),
            to: Some(TxKind::Call(Address::ZERO)),
            input: message.into(),
            ..Default::default()
        };

        // Sign the transaction and return directly since types match
        signer
            .build_and_sign_tx(request)
            .map_err(PayloadBuilderError::other)
    }

    pub fn add_builder_tx(
        &self,
        info: &mut ExecutionInfo,
        builder: &mut impl BlockBuilder<Primitives = Evm::Primitives>,
        db: &impl StateProvider,
        builder_tx_gas: u64,
        message: Vec<u8>,
    ) -> Option<()> {
        self.builder_signer()
            .map(|signer| {
                let base_fee = builder.evm_mut().block().basefee;
                let chain_id = self.chain_id();
                let builder_tx = self.signed_builder_tx(
                    db,
                    builder_tx_gas,
                    message,
                    signer,
                    base_fee,
                    chain_id,
                )?;
                let gas_used = builder.execute_transaction(builder_tx)?;
                info.cumulative_gas_used += gas_used;
                Ok(())
            })
            .transpose()
            .unwrap_or_else(|err: PayloadBuilderError| {
                warn!(target: "payload_builder", %err, "Failed to add builder transaction");
                None
            })
    }

    /// Calculates EIP 2718 builder transaction size
    pub fn estimate_builder_tx_da_size(
        &self,
        db: &impl StateProvider,
        base_fee: u64,
        builder_tx_gas: u64,
        message: Vec<u8>,
    ) -> Option<usize> {
        self.builder_signer()
            .map(|signer| {
                let chain_id = self.chain_id();
                // Create and sign the transaction
                let builder_tx = self.signed_builder_tx(
                    db,
                    builder_tx_gas,
                    message,
                    signer,
                    base_fee,
                    chain_id,
                )?;
                Ok(builder_tx.length())
            })
            .transpose()
            .unwrap_or_else(|err: PayloadBuilderError| {
                warn!(target: "payload_builder", %err, "Failed to add builder transaction");
                None
            })
    }

    /// Extracts commitment from access list entries, pointing to 0x420..022 and validates them
    /// against supervisor.
    ///
    /// If commitment present pre-interop tx rejected.
    ///
    /// Returns (is_valid, is_recoverable)
    pub fn is_cross_tx_valid(
        &self,
        tx: &PrimitivesTxTy<Evm::Primitives>,
        // TODO: after spec rebase we must make this field not optional
        client: Option<&SupervisorValidator>,
        safety_level: SafetyLevel,
        timestamp: u64,
        metrics: &OpRBuilderMetrics,
    ) -> (bool, bool) {
        if tx.access_list().is_none() {
            return (true, true);
        }
        let access_list = tx.access_list().unwrap();
        let inbox_entries = SupervisorValidator::parse_access_list(access_list)
            .cloned()
            .collect::<Vec<_>>();
        if !inbox_entries.is_empty() {
            metrics.inc_num_cross_chain_tx();
            if client.is_none() {
                return (false, true);
            }
            match self.validate_supervisor_messages(
                inbox_entries,
                client.unwrap(),
                safety_level,
                timestamp,
            ) {
                Ok(res) => match res {
                    Ok(()) => (true, true),
                    Err(err) => {
                        match err {
                            // TODO: we should add reconnecting to supervisor in case of disconnect
                            InteropTxValidatorError::SupervisorServerError(err) => {
                                warn!(target: "payload_builder", %err, ?tx, "Supervisor error, skipping.");
                                metrics.inc_num_cross_chain_tx_server_error();
                                (false, true)
                            }
                            InteropTxValidatorError::ValidationTimeout(_) => {
                                warn!(target: "payload_builder", %err, ?tx, "Cross tx validation timed out, skipping.");
                                metrics.inc_num_cross_chain_tx_timeout();
                                (false, true)
                            }
                            err => {
                                trace!(target: "payload_builder", %err, ?tx, "Cross tx rejected.");
                                metrics.inc_num_cross_chain_tx_fail();
                                // It's possible that transaction invalid now, but would be valid later.
                                // We should keep limited queue for transactions that could become valid.
                                // We should have the limit to ensure that builder won't get overwhelmed.
                                (false, false)
                            }
                        }
                    }
                },
                Err(err) => {
                    error!(target: "payload_builder", %err, ?tx, "Client side error during cross tx validation, skipping.");
                    (false, true)
                }
            }
        } else {
            (true, true)
        }
    }

    /// Validate inbox_entries against supervisor.
    pub fn validate_supervisor_messages(
        &self,
        inbox_entries: Vec<B256>,
        client: &SupervisorValidator,
        safety_level: SafetyLevel,
        timestamp: u64,
    ) -> Result<Result<(), InteropTxValidatorError>, PayloadBuilderError> {
        // For block building the timestamp should be `expected time of inclusion` and timeout 0
        let descriptor = ExecutingDescriptor::new(timestamp, None);
        let (channel_tx, rx) = std::sync::mpsc::channel();
        tokio::task::block_in_place(move || {
            let res = tokio::runtime::Handle::current().block_on(async {
                client
                    .validate_messages(inbox_entries.as_slice(), safety_level, descriptor)
                    .await
            });
            let _ = channel_tx.send(res);
        });
        rx.recv().map_err(|_| PayloadBuilderError::ChannelClosed)
    }
}

fn estimate_gas_for_builder_tx(input: Vec<u8>) -> u64 {
    // Count zero and non-zero bytes
    let (zero_bytes, nonzero_bytes) = input.iter().fold((0, 0), |(zeros, nonzeros), &byte| {
        if byte == 0 {
            (zeros + 1, nonzeros)
        } else {
            (zeros, nonzeros + 1)
        }
    });

    // Calculate gas cost (4 gas per zero byte, 16 gas per non-zero byte)
    let zero_cost = zero_bytes * 4;
    let nonzero_cost = nonzero_bytes * 16;

    zero_cost + nonzero_cost + 21_000
}
