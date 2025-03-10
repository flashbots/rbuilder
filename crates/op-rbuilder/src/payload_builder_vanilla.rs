use crate::generator::BlockPayloadJobGenerator;
use crate::generator::BuildArguments;
use crate::primitives::{
    estimate_gas_for_builder_tx, reth::OpPayloadBuilderCtx, signed_builder_tx,
};
use crate::{
    generator::{BlockCell, PayloadBuilder},
    metrics::OpRBuilderMetrics,
    primitives::reth::{ExecutedPayload, ExecutionInfo},
    tx_signer::Signer,
};
use alloy_consensus::constants::EMPTY_WITHDRAWALS;
use alloy_consensus::transaction::Recovered;
use alloy_consensus::{
    Eip658Value, Header, Transaction, TxEip1559, Typed2718, EMPTY_OMMER_ROOT_HASH,
};
use alloy_eips::merge::BEACON_NONCE;
use alloy_primitives::private::alloy_rlp::Encodable;
use alloy_primitives::{Address, Bytes, TxHash, TxKind, U256};
use alloy_rpc_types_engine::PayloadId;
use alloy_rpc_types_eth::Withdrawals;
use op_alloy_consensus::{OpDepositReceipt, OpTypedTransaction};
use reth::builder::{components::PayloadServiceBuilder, node::FullNodeTypes, BuilderContext};
use reth::core::primitives::InMemorySize;
use reth::payload::PayloadBuilderHandle;
use reth_basic_payload_builder::{
    BasicPayloadJobGeneratorConfig, BuildOutcome, BuildOutcomeKind, PayloadConfig,
};
use reth_chain_state::{ExecutedBlock, ExecutedBlockWithTrieUpdates};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_evm::{
    env::EvmEnv, system_calls::SystemCaller, ConfigureEvmEnv, ConfigureEvmFor, Database, Evm,
    EvmError, InvalidTxError, NextBlockEnvAttributes,
};
use reth_execution_types::ExecutionOutcome;
use reth_node_api::NodePrimitives;
use reth_node_api::NodeTypesWithEngine;
use reth_node_api::TxTy;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_consensus::calculate_receipt_root_no_memo_optimism;
use reth_optimism_evm::BasicOpReceiptBuilder;
use reth_optimism_evm::OpEvmConfig;
use reth_optimism_evm::{OpReceiptBuilder, ReceiptBuilderCtx};
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::OpEngineTypes;
use reth_optimism_payload_builder::config::{OpBuilderConfig, OpDAConfig};
use reth_optimism_payload_builder::OpPayloadPrimitives;
use reth_optimism_payload_builder::{
    error::OpPayloadBuilderError,
    payload::{OpBuiltPayload, OpPayloadBuilderAttributes},
};
use reth_optimism_primitives::{
    OpPrimitives, OpTransactionSigned, ADDRESS_L2_TO_L1_MESSAGE_PASSER,
};
use reth_payload_builder::PayloadBuilderService;
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_payload_util::BestPayloadTransactions;
use reth_payload_util::PayloadTransactions;
use reth_primitives::{transaction::SignedTransactionIntoRecoveredExt, BlockBody, SealedHeader};
use reth_primitives_traits::proofs;
use reth_primitives_traits::Block;
use reth_primitives_traits::RecoveredBlock;
use reth_primitives_traits::SignedTransaction;
use reth_provider::CanonStateSubscriptions;
use reth_provider::{
    HashedPostStateProvider, ProviderError, StateProviderFactory, StateRootProvider,
    StorageRootProvider,
};
use reth_revm::database::StateProviderDatabase;
use reth_transaction_pool::BestTransactionsAttributes;
use reth_transaction_pool::PoolTransaction;
use reth_transaction_pool::TransactionPool;
use revm::{
    db::{states::bundle_state::BundleRetention, State},
    primitives::{ExecutionResult, ResultAndState},
    DatabaseCommit,
};
use std::error::Error as StdError;
use std::{fmt::Display, sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::{info, trace, warn};

#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct CustomOpPayloadBuilder {
    builder_signer: Option<Signer>,
}

impl CustomOpPayloadBuilder {
    pub fn new(builder_signer: Option<Signer>) -> Self {
        Self { builder_signer }
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
{
    type PayloadBuilder = OpPayloadBuilderVanilla<Pool, Node::Provider, OpEvmConfig, OpPrimitives>;

    async fn build_payload_builder(
        &self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<Self::PayloadBuilder> {
        Ok(OpPayloadBuilderVanilla::new(
            OpEvmConfig::new(ctx.chain_spec()),
            self.builder_signer,
            pool,
            ctx.provider().clone(),
            Arc::new(BasicOpReceiptBuilder::default()),
        ))
    }

    fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        payload_builder: Self::PayloadBuilder,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypesWithEngine>::Engine>> {
        tracing::info!("Spawning a custom payload builder");
        let payload_job_config = BasicPayloadJobGeneratorConfig::default();

        let payload_generator = BlockPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            payload_job_config,
            payload_builder,
            false,
        );

        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical("custom payload builder service", Box::pin(payload_service));

        tracing::info!("Custom payload service started");

        Ok(payload_builder)
    }
}

impl<Pool, Client, EvmConfig, N, Txs> reth_basic_payload_builder::PayloadBuilder
    for OpPayloadBuilderVanilla<Pool, Client, EvmConfig, N, Txs>
where
    Pool: Clone + Send + Sync,
    Client: Clone + Send + Sync,
    EvmConfig: Clone + Send + Sync,
    N: NodePrimitives,
    Txs: Clone + Send + Sync,
{
    type Attributes = OpPayloadBuilderAttributes<N::SignedTx>;
    type BuiltPayload = OpBuiltPayload<N>;

    fn try_build(
        &self,
        _args: reth_basic_payload_builder::BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        unimplemented!()
    }

    fn build_empty_payload(
        &self,
        _config: reth_basic_payload_builder::PayloadConfig<
            Self::Attributes,
            reth_basic_payload_builder::HeaderForPayload<Self::BuiltPayload>,
        >,
    ) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        unimplemented!()
    }
}

/// Optimism's payload builder
#[derive(Debug, Clone)]
pub struct OpPayloadBuilderVanilla<Pool, Client, EvmConfig, N: NodePrimitives, Txs = ()> {
    /// The type responsible for creating the evm.
    pub evm_config: EvmConfig,
    /// The builder's signer key to use for an end of block tx
    pub builder_signer: Option<Signer>,
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
    /// Node primitive types.
    pub receipt_builder: Arc<dyn OpReceiptBuilder<N::SignedTx, Receipt = N::Receipt>>,
}

impl<Pool, Client, EvmConfig, N: NodePrimitives>
    OpPayloadBuilderVanilla<Pool, Client, EvmConfig, N>
{
    /// `OpPayloadBuilder` constructor.
    pub fn new(
        evm_config: EvmConfig,
        builder_signer: Option<Signer>,
        pool: Pool,
        client: Client,
        receipt_builder: Arc<dyn OpReceiptBuilder<N::SignedTx, Receipt = N::Receipt>>,
    ) -> Self {
        Self::with_builder_config(
            evm_config,
            builder_signer,
            pool,
            client,
            receipt_builder,
            Default::default(),
        )
    }

    pub fn with_builder_config(
        evm_config: EvmConfig,
        builder_signer: Option<Signer>,
        pool: Pool,
        client: Client,
        receipt_builder: Arc<dyn OpReceiptBuilder<N::SignedTx, Receipt = N::Receipt>>,
        config: OpBuilderConfig,
    ) -> Self {
        Self {
            pool,
            client,
            receipt_builder,
            config,
            evm_config,
            best_transactions: (),
            metrics: Default::default(),
            builder_signer,
        }
    }
}

impl<EvmConfig, Pool, Client, N, Txs> PayloadBuilder
    for OpPayloadBuilderVanilla<Pool, Client, EvmConfig, N, Txs>
where
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthChainSpec + OpHardforks> + Clone,
    N: OpPayloadPrimitives<_TX = OpTransactionSigned>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = N::SignedTx>>,
    EvmConfig: ConfigureEvmFor<N>,
    Txs: OpPayloadTransactions<Pool::Transaction>,
{
    type Attributes = OpPayloadBuilderAttributes<N::SignedTx>;
    type BuiltPayload = OpBuiltPayload<N>;

    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
        best_payload: BlockCell<Self::BuiltPayload>,
    ) -> Result<(), PayloadBuilderError> {
        let pool = self.pool.clone();
        let block_build_start_time = Instant::now();

        match self.build_payload(
            args,
            |attrs| {
                #[allow(clippy::unit_arg)]
                self.best_transactions
                    .best_transactions(pool.clone(), attrs)
            },
            |hashes| {
                #[allow(clippy::unit_arg)]
                self.best_transactions.remove_reverted(pool.clone(), hashes)
            },
        )? {
            BuildOutcome::Better { payload, .. } => {
                best_payload.set(payload);
                self.metrics
                    .total_block_built_duration
                    .record(block_build_start_time.elapsed());
                self.metrics.block_built_success.increment(1);
                Ok(())
            }
            BuildOutcome::Freeze(payload) => {
                best_payload.set(payload);
                self.metrics
                    .total_block_built_duration
                    .record(block_build_start_time.elapsed());
                Ok(())
            }
            BuildOutcome::Cancelled => {
                tracing::warn!("Payload build cancelled");
                Err(PayloadBuilderError::MissingPayload)
            }
            _ => {
                tracing::warn!("No better payload found");
                Err(PayloadBuilderError::MissingPayload)
            }
        }
    }
}

impl<Pool, Client, EvmConfig, N, T> OpPayloadBuilderVanilla<Pool, Client, EvmConfig, N, T>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = N::SignedTx>>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthChainSpec + OpHardforks>,
    N: OpPayloadPrimitives<_TX = OpTransactionSigned>,
    EvmConfig: ConfigureEvmFor<N>,
{
    /// Constructs an Optimism payload from the transactions sent via the
    /// Payload attributes by the sequencer. If the `no_tx_pool` argument is passed in
    /// the payload attributes, the transaction pool will be ignored and the only transactions
    /// included in the payload will be those sent through the attributes.
    ///
    /// Given build arguments including an Optimism client, transaction pool,
    /// and configuration, this function creates a transaction payload. Returns
    /// a result indicating success with the payload or an error in case of failure.
    fn build_payload<'a, Txs>(
        &self,
        args: BuildArguments<OpPayloadBuilderAttributes<N::SignedTx>, OpBuiltPayload<N>>,
        best: impl FnOnce(BestTransactionsAttributes) -> Txs + Send + Sync + 'a,
        remove_reverted: impl FnOnce(Vec<TxHash>),
    ) -> Result<BuildOutcome<OpBuiltPayload<N>>, PayloadBuilderError>
    where
        Txs: PayloadTransactions<Transaction: PoolTransaction<Consensus = N::SignedTx>>,
    {
        let evm_env = self
            .evm_env(&args.config.attributes, &args.config.parent_header)
            .map_err(PayloadBuilderError::other)?;

        let BuildArguments {
            mut cached_reads,
            config,
            cancel,
        } = args;

        let ctx = OpPayloadBuilderCtx {
            evm_config: self.evm_config.clone(),
            da_config: self.config.da_config.clone(),
            chain_spec: self.client.chain_spec(),
            config,
            evm_env,
            cancel,
            receipt_builder: self.receipt_builder.clone(),
            builder_signer: self.builder_signer,
            metrics: Default::default(),
        };

        let builder = OpBuilder::new(best, remove_reverted);

        let state_provider = self.client.state_by_block_hash(ctx.parent().hash())?;
        let state = StateProviderDatabase::new(state_provider);

        if ctx.attributes().no_tx_pool {
            let db = State::builder()
                .with_database(state)
                .with_bundle_update()
                .build();
            builder.build(db, ctx)
        } else {
            // sequencer mode we can reuse cachedreads from previous runs
            let db = State::builder()
                .with_database(cached_reads.as_db_mut(state))
                .with_bundle_update()
                .build();
            builder.build(db, ctx)
        }
        .map(|out| out.with_cached_reads(cached_reads))
    }

    /// Returns the configured [`EvmEnv`] for the targeted payload
    /// (that has the `parent` as its parent).
    pub fn evm_env(
        &self,
        attributes: &OpPayloadBuilderAttributes<N::SignedTx>,
        parent: &Header,
    ) -> Result<EvmEnv<EvmConfig::Spec>, EvmConfig::Error> {
        let next_attributes = NextBlockEnvAttributes {
            timestamp: attributes.timestamp(),
            suggested_fee_recipient: attributes.suggested_fee_recipient(),
            prev_randao: attributes.prev_randao(),
            gas_limit: attributes.gas_limit.unwrap_or(parent.gas_limit),
        };
        self.evm_config.next_evm_env(parent, next_attributes)
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
    remove_reverted: Box<dyn FnOnce(Vec<TxHash>) + 'a>,
}

impl<'a, Txs> OpBuilder<'a, Txs> {
    fn new(
        best: impl FnOnce(BestTransactionsAttributes) -> Txs + Send + Sync + 'a,
        remove_reverted: impl FnOnce(Vec<TxHash>) + 'a,
    ) -> Self {
        Self {
            best: Box::new(best),
            remove_reverted: Box::new(remove_reverted),
        }
    }
}

impl<Txs> OpBuilder<'_, Txs> {
    /// Executes the payload and returns the outcome.
    pub fn execute<EvmConfig, ChainSpec, N, DB, P>(
        self,
        state: &mut State<DB>,
        ctx: &OpPayloadBuilderCtx<EvmConfig, ChainSpec, N>,
    ) -> Result<BuildOutcomeKind<ExecutedPayload<N>>, PayloadBuilderError>
    where
        N: OpPayloadPrimitives<_TX = OpTransactionSigned>,
        Txs: PayloadTransactions<Transaction: PoolTransaction<Consensus = N::SignedTx>>,
        EvmConfig: ConfigureEvmFor<N>,
        ChainSpec: EthChainSpec + OpHardforks,
        DB: Database<Error = ProviderError> + AsRef<P>,
        P: StorageRootProvider,
    {
        let Self {
            best,
            remove_reverted,
        } = self;
        info!(target: "payload_builder", id=%ctx.payload_id(), parent_header = ?ctx.parent().hash(), parent_number = ctx.parent().number, "building new payload");

        // 1. apply eip-4788 pre block contract call
        ctx.apply_pre_beacon_root_contract_call(state)?;

        // 2. ensure create2deployer is force deployed
        ctx.ensure_create2_deployer(state)?;

        let sequencer_tx_start_time = Instant::now();

        // 3. execute sequencer transactions
        let mut info = ctx.execute_sequencer_transactions(state)?;

        ctx.metrics
            .sequencer_tx_duration
            .record(sequencer_tx_start_time.elapsed());

        // 4. if mem pool transactions are requested we execute them

        // gas reserved for builder tx
        let message = format!("Block Number: {}", ctx.block_number())
            .as_bytes()
            .to_vec();
        let builder_tx_gas = ctx
            .builder_signer()
            .map_or(0, |_| estimate_gas_for_builder_tx(message.clone()));
        let block_gas_limit = ctx.block_gas_limit() - builder_tx_gas;
        // Save some space in the block_da_limit for builder tx
        let builder_tx_da_size = ctx
            .estimate_builder_tx_da_size(state, builder_tx_gas, message.clone())
            .unwrap_or(0);
        let block_da_limit = ctx
            .da_config
            .max_da_block_size()
            .map(|da_size| da_size - builder_tx_da_size as u64);
        // Check that it's possible to create builder tx, considering max_da_tx_size, otherwise panic
        let tx_da_limit = ctx.da_config.max_da_tx_size();
        if let Some(tx_da_limit) = tx_da_limit {
            // Panic indicate max_da_tx_size misconfiguration
            assert!(
                tx_da_limit >= builder_tx_da_size as u64,
                "The configured da_config.max_da_tx_size is too small to accommodate builder tx."
            );
        }

        if !ctx.attributes().no_tx_pool {
            let best_txs_start_time = Instant::now();
            let best_txs = best(ctx.best_transaction_attributes());
            ctx.metrics
                .transaction_pool_fetch_duration
                .record(best_txs_start_time.elapsed());
            if ctx
                .execute_best_transactions(
                    &mut info,
                    state,
                    best_txs,
                    block_gas_limit,
                    block_da_limit,
                    tx_da_limit,
                )?
                .is_some()
            {
                return Ok(BuildOutcomeKind::Cancelled);
            }
        }

        // Add builder tx to the block
        ctx.add_builder_tx(&mut info, state, builder_tx_gas, message);

        let state_merge_start_time = Instant::now();

        // merge all transitions into bundle state, this would apply the withdrawal balance changes
        // and 4788 contract call
        state.merge_transitions(BundleRetention::Reverts);

        ctx.metrics
            .state_transition_merge_duration
            .record(state_merge_start_time.elapsed());
        ctx.metrics
            .payload_num_tx
            .record(info.executed_transactions.len() as f64);

        let withdrawals_root = if ctx.is_isthmus_active() {
            // withdrawals root field in block header is used for storage root of L2 predeploy
            // `l2tol1-message-passer`
            Some(
                state
                    .database
                    .as_ref()
                    .storage_root(ADDRESS_L2_TO_L1_MESSAGE_PASSER, Default::default())?,
            )
        } else if ctx.is_canyon_active() {
            Some(EMPTY_WITHDRAWALS)
        } else {
            None
        };

        remove_reverted(info.reverted_tx_hashes.iter().copied().collect());

        let payload = ExecutedPayload {
            info,
            withdrawals_root,
        };

        Ok(BuildOutcomeKind::Better { payload })
    }

    /// Builds the payload on top of the state.
    pub fn build<EvmConfig, ChainSpec, N, DB, P>(
        self,
        mut state: State<DB>,
        ctx: OpPayloadBuilderCtx<EvmConfig, ChainSpec, N>,
    ) -> Result<BuildOutcomeKind<OpBuiltPayload<N>>, PayloadBuilderError>
    where
        EvmConfig: ConfigureEvmFor<N>,
        ChainSpec: EthChainSpec + OpHardforks,
        N: OpPayloadPrimitives<_TX = OpTransactionSigned>,
        Txs: PayloadTransactions<Transaction: PoolTransaction<Consensus = N::SignedTx>>,
        DB: Database<Error = ProviderError> + AsRef<P>,
        P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    {
        let ExecutedPayload {
            info,
            withdrawals_root,
        } = match self.execute(&mut state, &ctx)? {
            BuildOutcomeKind::Better { payload } | BuildOutcomeKind::Freeze(payload) => payload,
            BuildOutcomeKind::Cancelled => return Ok(BuildOutcomeKind::Cancelled),
            BuildOutcomeKind::Aborted { fees } => return Ok(BuildOutcomeKind::Aborted { fees }),
        };

        let block_number = ctx.block_number();
        let execution_outcome = ExecutionOutcome::new(
            state.take_bundle(),
            vec![info.receipts],
            block_number,
            Vec::new(),
        );
        let receipts_root = execution_outcome
            .generic_receipts_root_slow(block_number, |receipts| {
                calculate_receipt_root_no_memo_optimism(
                    receipts,
                    &ctx.chain_spec,
                    ctx.attributes().timestamp(),
                )
            })
            .expect("Number is in range");
        let logs_bloom = execution_outcome
            .block_logs_bloom(block_number)
            .expect("Number is in range");

        // calculate the state root
        let state_root_start_time = Instant::now();

        let state_provider = state.database.as_ref();
        let hashed_state = state_provider.hashed_post_state(execution_outcome.state());
        let (state_root, trie_output) = {
            state
                .database
                .as_ref()
                .state_root_with_updates(hashed_state.clone())
                .inspect_err(|err| {
                    warn!(target: "payload_builder",
                    parent_header=%ctx.parent().hash(),
                        %err,
                        "failed to calculate state root for payload"
                    );
                })?
        };

        ctx.metrics
            .state_root_calculation_duration
            .record(state_root_start_time.elapsed());

        // create the block header
        let transactions_root = proofs::calculate_transaction_root(&info.executed_transactions);

        // OP doesn't support blobs/EIP-4844.
        // https://specs.optimism.io/protocol/exec-engine.html#ecotone-disable-blob-transactions
        // Need [Some] or [None] based on hardfork to match block hash.
        let (excess_blob_gas, blob_gas_used) = ctx.blob_fields();
        let extra_data = ctx.extra_data()?;

        let header = Header {
            parent_hash: ctx.parent().hash(),
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: ctx.evm_env.block_env.coinbase,
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root,
            logs_bloom,
            timestamp: ctx.attributes().payload_attributes.timestamp,
            mix_hash: ctx.attributes().payload_attributes.prev_randao,
            nonce: BEACON_NONCE.into(),
            base_fee_per_gas: Some(ctx.base_fee()),
            number: ctx.parent().number + 1,
            gas_limit: ctx.block_gas_limit(),
            difficulty: U256::ZERO,
            gas_used: info.cumulative_gas_used,
            extra_data,
            parent_beacon_block_root: ctx.attributes().payload_attributes.parent_beacon_block_root,
            blob_gas_used,
            excess_blob_gas,
            requests_hash: None,
        };

        // seal the block
        let block = N::Block::new(
            header,
            BlockBody {
                transactions: info.executed_transactions,
                ommers: vec![],
                withdrawals: ctx.withdrawals().cloned(),
            },
        );

        let sealed_block = Arc::new(block.seal_slow());
        info!(target: "payload_builder", id=%ctx.attributes().payload_id(), "sealed built block");

        // create the executed block data
        let executed: ExecutedBlockWithTrieUpdates<N> = ExecutedBlockWithTrieUpdates {
            block: ExecutedBlock {
                recovered_block: Arc::new(RecoveredBlock::new_sealed(
                    sealed_block.as_ref().clone(),
                    info.executed_senders,
                )),
                execution_output: Arc::new(execution_outcome),
                hashed_state: Arc::new(hashed_state),
            },
            trie: Arc::new(trie_output),
        };

        let no_tx_pool = ctx.attributes().no_tx_pool;

        let payload = OpBuiltPayload::new(
            ctx.payload_id(),
            sealed_block,
            info.total_fees,
            Some(executed),
        );

        ctx.metrics
            .payload_byte_size
            .record(payload.block().size() as f64);

        if no_tx_pool {
            // if `no_tx_pool` is set only transactions from the payload attributes will be included
            // in the payload. In other words, the payload is deterministic and we can
            // freeze it once we've successfully built it.
            Ok(BuildOutcomeKind::Freeze(payload))
        } else {
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

    /// Removes reverted transactions from the tx pool
    fn remove_reverted<Pool: TransactionPool<Transaction = Transaction>>(
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

    fn remove_reverted<Pool: TransactionPool<Transaction = T>>(
        &self,
        pool: Pool,
        hashes: Vec<TxHash>,
    ) {
        pool.remove_transactions(hashes);
    }
}
