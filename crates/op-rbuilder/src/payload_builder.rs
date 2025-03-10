use std::{fmt::Display, sync::Arc, sync::Mutex};

use crate::{
    generator::{BlockCell, BlockPayloadJobGenerator, BuildArguments, PayloadBuilder},
    primitives::reth::{ExecutedPayload, ExecutionInfo},
    tx_signer::Signer,
};
use alloy_consensus::{Eip658Value, Header, Transaction, Typed2718, EMPTY_OMMER_ROOT_HASH};
use alloy_eips::merge::BEACON_NONCE;
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, Bytes, TxHash, B256, U256};
use alloy_rpc_types_engine::PayloadId;
use alloy_rpc_types_eth::Withdrawals;
use op_alloy_consensus::OpDepositReceipt;
use reth::builder::{components::PayloadServiceBuilder, node::FullNodeTypes, BuilderContext};
use reth::payload::PayloadBuilderHandle;
use reth_basic_payload_builder::commit_withdrawals;
use reth_basic_payload_builder::BasicPayloadJobGeneratorConfig;
use reth_basic_payload_builder::{BuildOutcome, PayloadConfig};
use reth_chainspec::ChainSpecProvider;
use reth_chainspec::EthChainSpec;
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
use reth_optimism_payload_builder::error::OpPayloadBuilderError;
use reth_optimism_payload_builder::payload::{OpBuiltPayload, OpPayloadBuilderAttributes};
use reth_optimism_payload_builder::OpPayloadPrimitives;
use reth_optimism_primitives::{OpPrimitives, ADDRESS_L2_TO_L1_MESSAGE_PASSER};
use reth_optimism_primitives::OpTransactionSigned;
use reth_payload_builder::PayloadBuilderService;
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_payload_util::BestPayloadTransactions;
use reth_payload_util::PayloadTransactions;
use reth_primitives::{transaction::SignedTransactionIntoRecoveredExt, BlockBody, SealedHeader};
use reth_primitives_traits::proofs;
use reth_primitives_traits::Block as _;
use reth_provider::CanonStateSubscriptions;
use reth_provider::StorageRootProvider;
use reth_provider::{
    HashedPostStateProvider, ProviderError, StateProviderFactory, StateRootProvider,
};
use reth_revm::database::StateProviderDatabase;
use reth_transaction_pool::PoolTransaction;
use reth_transaction_pool::{BestTransactionsAttributes, TransactionPool};
use revm::primitives::ExecutionResult;
use revm::{
    db::{states::bundle_state::BundleRetention, BundleState, State},
    primitives::ResultAndState,
    DatabaseCommit,
};
use rollup_boost::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblocksPayloadV1,
};
use serde_json::Value;
use std::error::Error as StdError;
use alloy_consensus::constants::EMPTY_WITHDRAWALS;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use futures_util::FutureExt;
use futures_util::SinkExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::WebSocketStream;
use crate::primitives::reth::OpPayloadBuilderCtx;

#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct CustomOpPayloadBuilder {
    #[allow(dead_code)]
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
    type PayloadBuilder = OpPayloadBuilder<Pool, Node::Provider, OpEvmConfig, OpPrimitives>;

    async fn build_payload_builder(
        &self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<Self::PayloadBuilder> {
        Ok(OpPayloadBuilder::new(
            OpEvmConfig::new(ctx.chain_spec()),
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
            true,
        );

        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical("custom payload builder service", Box::pin(payload_service));

        tracing::info!("Custom payload service started");

        Ok(payload_builder)
    }
}

impl<Pool, Client, EvmConfig, N> reth_basic_payload_builder::PayloadBuilder
    for OpPayloadBuilder<Pool, Client, EvmConfig, N>
where
    Pool: Clone + Send + Sync,
    Client: Clone + Send + Sync,
    EvmConfig: Clone + Send + Sync,
    N: NodePrimitives,
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
pub struct OpPayloadBuilder<Pool, Client, EvmConfig, N: NodePrimitives> {
    /// The type responsible for creating the evm.
    pub evm_config: EvmConfig,
    /// The transaction pool
    pub pool: Pool,
    /// Node client
    pub client: Client,
    /// Channel sender for publishing messages
    pub tx: mpsc::UnboundedSender<String>,
    /// Node primitive types.
    pub receipt_builder: Arc<dyn OpReceiptBuilder<N::SignedTx, Receipt = N::Receipt>>,
}

impl<Pool, Client, EvmConfig, N: NodePrimitives> OpPayloadBuilder<Pool, Client, EvmConfig, N> {
    /// `OpPayloadBuilder` constructor.
    pub fn new(
        evm_config: EvmConfig,
        pool: Pool,
        client: Client,
        receipt_builder: Arc<dyn OpReceiptBuilder<N::SignedTx, Receipt = N::Receipt>>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let subscribers = Arc::new(Mutex::new(Vec::new()));

        Self::publish_task(rx, subscribers.clone());

        tokio::spawn(async move {
            Self::start_ws(subscribers, "127.0.0.1:1111").await;
        });

        Self {
            evm_config,
            pool,
            client,
            tx,
            receipt_builder,
        }
    }

    /// Start the WebSocket server
    pub async fn start_ws(subscribers: Arc<Mutex<Vec<WebSocketStream<TcpStream>>>>, addr: &str) {
        let listener = TcpListener::bind(addr).await.unwrap();
        let subscribers = subscribers.clone();

        tracing::info!("Starting WebSocket server on {}", addr);

        while let Ok((stream, _)) = listener.accept().await {
            tracing::info!("Accepted websocket connection");
            let subscribers = subscribers.clone();

            tokio::spawn(async move {
                match accept_async(stream).await {
                    Ok(ws_stream) => {
                        let mut subs = subscribers.lock().unwrap();
                        subs.push(ws_stream);
                    }
                    Err(e) => eprintln!("Error accepting websocket connection: {}", e),
                }
            });
        }
    }

    /// Background task that handles publishing messages to WebSocket subscribers
    fn publish_task(
        mut rx: mpsc::UnboundedReceiver<String>,
        subscribers: Arc<Mutex<Vec<WebSocketStream<TcpStream>>>>,
    ) {
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                let mut subscribers = subscribers.lock().unwrap();

                // Remove disconnected subscribers and send message to connected ones
                subscribers.retain_mut(|ws_stream| {
                    let message = message.clone();
                    async move {
                        ws_stream
                            .send(tokio_tungstenite::tungstenite::Message::Text(
                                message.into(),
                            ))
                            .await
                            .is_ok()
                    }
                    .now_or_never()
                    .unwrap_or(false)
                });
            }
        });
    }
}

impl<Pool, Client, EvmConfig, N> OpPayloadBuilder<Pool, Client, EvmConfig, N>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = N::SignedTx>>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthChainSpec + OpHardforks>,
    N: OpPayloadPrimitives<_TX = OpTransactionSigned>,
    EvmConfig: ConfigureEvmFor<N>,
{
    /// Send a message to be published
    pub fn send_message(&self, message: String) -> Result<(), Box<dyn std::error::Error>> {
        self.tx.send(message).map_err(|e| e.into())
    }

    /// Constructs an Optimism payload from the transactions sent via the
    /// Payload attributes by the sequencer. If the `no_tx_pool` argument is passed in
    /// the payload attributes, the transaction pool will be ignored and the only transactions
    /// included in the payload will be those sent through the attributes.
    ///
    /// Given build arguments including an Optimism client, transaction pool,
    /// and configuration, this function creates a transaction payload. Returns
    /// a result indicating success with the payload or an error in case of failure.
    fn build_payload<'a>(
        &self,
        args: BuildArguments<OpPayloadBuilderAttributes<N::SignedTx>, OpBuiltPayload<N>>,
        best_payload: BlockCell<OpBuiltPayload<N>>,
    ) -> Result<(), PayloadBuilderError> {
        let evm_env = self
            .evm_env(&args.config.attributes, &args.config.parent_header)
            .map_err(PayloadBuilderError::other)?;

        let BuildArguments { config, cancel, .. } = args;

        let ctx = OpPayloadBuilderCtx {
            evm_config: self.evm_config.clone(),
            chain_spec: self.client.chain_spec(),
            config,
            evm_env,
            cancel,
            receipt_builder: self.receipt_builder.clone(),
            metrics: Default::default(),
            // TODO: unused for not
            da_config: Default::default(),
        };

        let state_provider = self.client.state_by_block_hash(ctx.parent().hash())?;
        let state = StateProviderDatabase::new(&state_provider);

        let mut db = State::builder()
            .with_database(state)
            .with_bundle_update()
            .build();

        // 1. execute the pre steps and seal an early block with that
        let mut info = execute_pre_steps(&mut db, &ctx)?;
        let (payload, fb_payload, mut bundle_state) = build_block(db, &ctx, &mut info)?;

        best_payload.set(payload.clone());
        let _ = self.send_message(serde_json::to_string(&fb_payload).unwrap_or_default());

        tracing::info!(target: "payload_builder", "Fallback block built");

        if ctx.attributes().no_tx_pool {
            tracing::info!(
                target: "payload_builder",
                "No transaction pool, skipping transaction pool processing",
            );

            // return early since we don't need to build a block with transactions from the pool
            return Ok(());
        }

        // Right now it assumes a 1 second block time (TODO)
        let gas_per_batch = ctx.block_gas_limit() / 4;
        let mut total_gas_per_batch = gas_per_batch;

        let mut flashblock_count = 0;

        // 2. loop every n time and try to build an increasing block
        loop {
            if ctx.cancel.is_cancelled() {
                tracing::info!(
                    target: "payload_builder",
                    "Job cancelled, stopping payload building",
                );
                // if the job was cancelled, stop
                return Ok(());
            }

            println!(
                "Building flashblock {} {}",
                ctx.payload_id(),
                flashblock_count,
            );

            tracing::info!(
                target: "payload_builder",
                "Building flashblock {}",
                flashblock_count,
            );

            let state = StateProviderDatabase::new(&state_provider);

            let mut db = State::builder()
                .with_database(state)
                .with_bundle_update()
                .with_bundle_prestate(bundle_state)
                .build();

            let best_txs = BestPayloadTransactions::new(
                self.pool
                    .best_transactions_with_attributes(ctx.best_transaction_attributes()),
            );
            // TODO: flashblocks doesn't have DA limits implemented, for now we pass None, None
            ctx.execute_best_transactions(&mut info, &mut db, best_txs, total_gas_per_batch, None, None)?;

            if ctx.cancel.is_cancelled() {
                tracing::info!(
                    target: "payload_builder",
                    "Job cancelled, stopping payload building",
                );
                // if the job was cancelled, stop
                return Ok(());
            }

            let (payload, mut fb_payload, new_bundle_state) = build_block(db, &ctx, &mut info)?;

            best_payload.set(payload.clone());

            fb_payload.index = flashblock_count + 1; // we do this because the fallback block is index 0
            fb_payload.base = None;
            let _ = self.send_message(serde_json::to_string(&fb_payload).unwrap_or_default());

            bundle_state = new_bundle_state;
            total_gas_per_batch += gas_per_batch;
            flashblock_count += 1;

            std::thread::sleep(std::time::Duration::from_millis(250));
        }
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

impl<EvmConfig, Pool, Client, N> PayloadBuilder for OpPayloadBuilder<Pool, Client, EvmConfig, N>
where
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthChainSpec + OpHardforks> + Clone,
    N: OpPayloadPrimitives<_TX = OpTransactionSigned>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = N::SignedTx>>,
    EvmConfig: ConfigureEvmFor<N>,
{
    type Attributes = OpPayloadBuilderAttributes<N::SignedTx>;
    type BuiltPayload = OpBuiltPayload<N>;

    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
        best_payload: BlockCell<Self::BuiltPayload>,
    ) -> Result<(), PayloadBuilderError> {
        self.build_payload(args, best_payload)
    }
}

pub fn build_block<EvmConfig, ChainSpec, N, DB, P>(
    mut state: State<DB>,
    ctx: &OpPayloadBuilderCtx<EvmConfig, ChainSpec, N>,
    info: &mut ExecutionInfo<N>,
) -> Result<(OpBuiltPayload<N>, FlashblocksPayloadV1, BundleState), PayloadBuilderError>
where
    EvmConfig: ConfigureEvmFor<N>,
    ChainSpec: EthChainSpec + OpHardforks,
    N: OpPayloadPrimitives<_TX = OpTransactionSigned>,
    DB: Database<Error = ProviderError> + AsRef<P>,
    P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
{
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

    // TODO: We must run this only once per block, but we are running it on every flashblock
    // merge all transitions into bundle state, this would apply the withdrawal balance changes
    // and 4788 contract call
    state.merge_transitions(BundleRetention::Reverts);

    let new_bundle = state.take_bundle();

    let block_number = ctx.block_number();
    assert_eq!(block_number, ctx.parent().number + 1);

    let execution_outcome = ExecutionOutcome::new(
        new_bundle.clone(),
        vec![info.receipts.clone()],
        block_number,
        vec![],
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

    // // calculate the state root
    let state_provider = state.database.as_ref();
    let hashed_state = state_provider.hashed_post_state(execution_outcome.state());
    let (state_root, _trie_output) = {
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
            transactions: info.executed_transactions.clone(),
            ommers: vec![],
            withdrawals: ctx.withdrawals().cloned(),
        },
    );

    let sealed_block = Arc::new(block.seal_slow());
    debug!(target: "payload_builder", ?sealed_block, "sealed built block");

    let block_hash = sealed_block.hash();

    // pick the new transactions from the info field and update the last flashblock index
    let new_transactions = info.executed_transactions[info.last_flashblock_index..].to_vec();
    info.last_flashblock_index = info.executed_transactions.len();

    let new_transactions_encoded = new_transactions
        .into_iter()
        .map(|tx| tx.encoded_2718().into())
        .collect::<Vec<_>>();

    // Prepare the flashblocks message
    let fb_payload = FlashblocksPayloadV1 {
        payload_id: ctx.payload_id(),
        index: 0,
        base: Some(ExecutionPayloadBaseV1 {
            parent_beacon_block_root: ctx
                .attributes()
                .payload_attributes
                .parent_beacon_block_root
                .unwrap(),
            parent_hash: ctx.parent().hash(),
            fee_recipient: ctx.attributes().suggested_fee_recipient(),
            prev_randao: ctx.attributes().payload_attributes.prev_randao,
            block_number: ctx.parent().number + 1,
            gas_limit: ctx.block_gas_limit(),
            timestamp: ctx.attributes().payload_attributes.timestamp,
            extra_data: ctx.extra_data()?,
            base_fee_per_gas: ctx.base_fee().try_into().unwrap(),
        }),
        diff: ExecutionPayloadFlashblockDeltaV1 {
            state_root,
            receipts_root,
            logs_bloom,
            gas_used: info.cumulative_gas_used,
            block_hash,
            transactions: new_transactions_encoded,
            withdrawals: ctx.withdrawals().cloned().unwrap_or_default().to_vec(),
        },
        metadata: Value::Null,
    };

    Ok((
        OpBuiltPayload::new(
            ctx.payload_id(),
            sealed_block,
            info.total_fees,
            // This must be set to NONE for now because we are doing merge transitions on every flashblock
            // when it should only happen once per block, thus, it returns a confusing state back to op-reth.
            // We can live without this for now because Op syncs up the executed block using new_payload
            // calls, but eventually we would want to return the executed block here.
            None,
        ),
        fb_payload,
        new_bundle,
    ))
}

fn execute_pre_steps<EvmConfig, ChainSpec, N, DB>(
    state: &mut State<DB>,
    ctx: &OpPayloadBuilderCtx<EvmConfig, ChainSpec, N>,
) -> Result<ExecutionInfo<N>, PayloadBuilderError>
where
    EvmConfig: ConfigureEvmFor<N>,
    ChainSpec: EthChainSpec + OpHardforks,
    N: OpPayloadPrimitives<_TX = OpTransactionSigned>,
    DB: Database<Error = ProviderError>,
{
    // 1. apply eip-4788 pre block contract call
    ctx.apply_pre_beacon_root_contract_call(state)?;

    // 2. ensure create2deployer is force deployed
    ctx.ensure_create2_deployer(state)?;

    // 3. execute sequencer transactions
    let info = ctx.execute_sequencer_transactions(state)?;

    Ok(info)
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
