use alloy_consensus::{Eip658Value, Header, Transaction, TxReceipt};
use alloy_eips::{Encodable2718, Typed2718};
use alloy_op_evm::{
    block::{receipt_builder::OpReceiptBuilder, OpAlloyReceiptBuilder},
    OpBlockExecutionCtx, OpBlockExecutor, OpEvmFactory,
};
use alloy_op_hardforks::OpChainHardforks;
use op_alloy_consensus::{EIP1559ParamError, OpDepositReceipt};
use op_revm::{transaction::deposit::DEPOSIT_TRANSACTION_TYPE, OpSpecId, OpTransaction};
use reth::builder::{components::ExecutorBuilder, BuilderContext};
use reth_chainspec::EthChainSpec;
use reth_evm::{
    block::{
        BlockExecutionError, BlockExecutor, BlockExecutorFactory, BlockExecutorFor,
        BlockValidationError, StateChangeSource,
    },
    eth::receipt_builder::ReceiptBuilderCtx,
    ConfigureEvm, Database, Evm, EvmEnv, EvmFactory, FromRecoveredTx, InvalidTxError, OnStateHook,
};
use reth_node_api::{FullNodeTypes, NodePrimitives, NodeTypes};
use reth_node_ethereum::BasicBlockExecutorProvider;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::{
    OpBlockAssembler, OpEvmConfig, OpNextBlockEnvAttributes, OpRethReceiptBuilder,
};
use reth_optimism_forks::OpHardforks;
use reth_optimism_primitives::{DepositReceipt, OpPrimitives};
use reth_primitives::{Recovered, SealedBlock, SealedHeader};
use reth_primitives_traits::SignedTransaction;
use reth_provider::BlockExecutionResult;
use reth_revm::State;
use revm::{
    context::{result::ResultAndState, TxEnv},
    DatabaseCommit, Inspector,
};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
#[error("Reverting tx error: {message}")]
struct TransactionRevertedError {
    message: String,
}

// Implement the InvalidTxError trait for it
impl InvalidTxError for TransactionRevertedError {
    fn is_nonce_too_low(&self) -> bool {
        false
    }
}

/// A regular optimism evm and executor builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct OpRbuilderExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for OpRbuilderExecutorBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = OpChainSpec, Primitives = OpPrimitives>>,
{
    type EVM = OpRbuilderEvmConfig;
    type Executor = BasicBlockExecutorProvider<Self::EVM>;

    async fn build_evm(
        self,
        ctx: &BuilderContext<Node>,
    ) -> eyre::Result<(Self::EVM, Self::Executor)> {
        let evm_config =
            OpRbuilderEvmConfig::new(ctx.chain_spec(), OpRethReceiptBuilder::default());
        let executor = BasicBlockExecutorProvider::new(evm_config.clone());
        Ok((evm_config, executor))
    }
}

#[derive(Debug, Clone)]
pub struct OpRbuilderEvmConfig<
    ChainSpec = OpChainSpec,
    N: NodePrimitives = OpPrimitives,
    R = OpRethReceiptBuilder,
> {
    pub executor_factory: OpRbuilderBlockExecutorFactory<R, Arc<ChainSpec>>,
    inner: OpEvmConfig<ChainSpec, N, R>,
}

impl<ChainSpec> OpRbuilderEvmConfig<ChainSpec> {
    /// Creates a new [`OpEvmConfig`] with the given chain spec for OP chains.
    pub fn optimism(chain_spec: Arc<ChainSpec>) -> Self {
        Self::new(chain_spec, OpRethReceiptBuilder::default())
    }
}

impl<ChainSpec, N: NodePrimitives, R> OpRbuilderEvmConfig<ChainSpec, N, R>
where
    R: OpReceiptBuilder<Receipt: DepositReceipt, Transaction: SignedTransaction> + Clone,
{
    /// Creates a new [`OpRbuilderEvmConfig`] with the given chain spec.
    pub fn new(chain_spec: Arc<ChainSpec>, receipt_builder: R) -> Self {
        Self {
            inner: OpEvmConfig::new(chain_spec.clone(), receipt_builder.clone()),
            executor_factory: OpRbuilderBlockExecutorFactory::new(
                receipt_builder,
                chain_spec,
                OpEvmFactory::default(),
            ),
        }
    }
}

impl<ChainSpec, N, R> ConfigureEvm for OpRbuilderEvmConfig<ChainSpec, N, R>
where
    ChainSpec: EthChainSpec + OpHardforks,
    N: NodePrimitives<
        Receipt = R::Receipt,
        SignedTx = R::Transaction,
        BlockHeader = Header,
        BlockBody = alloy_consensus::BlockBody<R::Transaction>,
        Block = alloy_consensus::Block<R::Transaction>,
    >,
    OpTransaction<TxEnv>: FromRecoveredTx<N::SignedTx>,
    R: OpReceiptBuilder<Receipt: DepositReceipt, Transaction: SignedTransaction>,
    OpEvmConfig<ChainSpec, N, R>: Send + Sync + Unpin + Clone,
    Self: Send + Sync + Unpin + Clone + 'static,
{
    type Primitives = N;
    type Error = EIP1559ParamError;
    type NextBlockEnvCtx = OpNextBlockEnvAttributes;
    type BlockExecutorFactory = OpRbuilderBlockExecutorFactory<R, Arc<ChainSpec>>;
    type BlockAssembler = OpBlockAssembler<ChainSpec>;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        self.inner.block_assembler()
    }

    fn evm_env(&self, header: &Header) -> EvmEnv<OpSpecId> {
        self.inner.evm_env(header)
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnv<OpSpecId>, Self::Error> {
        self.inner.next_evm_env(parent, attributes)
    }

    fn context_for_block(&self, block: &'_ SealedBlock<N::Block>) -> OpBlockExecutionCtx {
        self.inner.context_for_block(block)
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader<N::BlockHeader>,
        attributes: Self::NextBlockEnvCtx,
    ) -> OpBlockExecutionCtx {
        self.inner.context_for_next_block(parent, attributes)
    }
}

#[derive(Debug, Clone, Default, Copy)]
pub struct OpRbuilderBlockExecutorFactory<
    R = OpAlloyReceiptBuilder,
    Spec = OpChainHardforks,
    EvmFactory = OpEvmFactory,
> {
    /// Receipt builder.
    receipt_builder: R,
    /// Chain specification.
    spec: Spec,
    /// EVM factory.
    evm_factory: EvmFactory,
}

impl<R, Spec, EvmFactory> OpRbuilderBlockExecutorFactory<R, Spec, EvmFactory> {
    /// Creates a new [`OpRbuilderBlockExecutorFactory`] with the given spec, [`EvmFactory`], and
    /// [`OpReceiptBuilder`].
    pub const fn new(receipt_builder: R, spec: Spec, evm_factory: EvmFactory) -> Self {
        Self {
            receipt_builder,
            spec,
            evm_factory,
        }
    }
}

impl<R, Spec, EvmF> BlockExecutorFactory for OpRbuilderBlockExecutorFactory<R, Spec, EvmF>
where
    R: OpReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt>,
    Spec: OpHardforks,
    EvmF: EvmFactory<Tx: FromRecoveredTx<R::Transaction>>,
    Self: 'static,
{
    type EvmFactory = EvmF;
    type ExecutionCtx<'a> = OpBlockExecutionCtx;
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: EvmF::Evm<&'a mut State<DB>, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: Database + 'a,
        I: Inspector<EvmF::Context<&'a mut State<DB>>> + 'a,
    {
        OpRbuilderBlockExecutor {
            inner: OpBlockExecutor::new(evm, ctx, &self.spec, &self.receipt_builder),
        }
    }
}

pub struct OpRbuilderBlockExecutor<Evm, R: OpReceiptBuilder, Spec> {
    inner: OpBlockExecutor<Evm, R, Spec>,
}

impl<'db, DB, E, R, Spec> BlockExecutor for OpRbuilderBlockExecutor<E, R, Spec>
where
    DB: Database + 'db,
    E: Evm<DB = &'db mut State<DB>, Tx: FromRecoveredTx<R::Transaction>>,
    R: OpReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt>,
    Spec: OpHardforks,
{
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;
    type Evm = E;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn execute_transaction_with_result_closure(
        &mut self,
        tx: Recovered<&Self::Transaction>,
        f: impl FnOnce(&revm::context::result::ExecutionResult<<Self::Evm as Evm>::HaltReason>),
    ) -> Result<u64, BlockExecutionError> {
        let is_deposit = tx.ty() == DEPOSIT_TRANSACTION_TYPE;

        // The sum of the transaction’s gas limit, Tg, and the gas utilized in this block prior,
        // must be no greater than the block’s gasLimit.
        let block_available_gas = self.inner.evm.block().gas_limit - self.inner.gas_used;
        if tx.gas_limit() > block_available_gas && (self.inner.is_regolith || !is_deposit) {
            return Err(
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: tx.gas_limit(),
                    block_available_gas,
                }
                .into(),
            );
        }

        // Cache the depositor account prior to the state transition for the deposit nonce.
        //
        // Note that this *only* needs to be done post-regolith hardfork, as deposit nonces
        // were not introduced in Bedrock. In addition, regular transactions don't have deposit
        // nonces, so we don't need to touch the DB for those.
        let depositor = (self.inner.is_regolith && is_deposit)
            .then(|| {
                self.inner
                    .evm
                    .db_mut()
                    .load_cache_account(tx.signer())
                    .map(|acc| acc.account_info().unwrap_or_default())
            })
            .transpose()
            .map_err(BlockExecutionError::other)?;

        let hash = tx.trie_hash();

        // Execute transaction.
        let result_and_state = self
            .inner
            .evm
            .transact(tx)
            .map_err(move |err| BlockExecutionError::evm(err, hash))?;

        if !result_and_state.result.is_success() {
            return Err(BlockValidationError::InvalidTx {
                hash,
                error: Box::new(TransactionRevertedError {
                    message: "transaction reverted".to_string(), // TODO: add more context on error
                }),
            }
            .into());
        }

        self.inner.system_caller.on_state(
            StateChangeSource::Transaction(self.inner.receipts.len()),
            &result_and_state.state,
        );
        let ResultAndState { result, state } = result_and_state;

        f(&result);

        let gas_used = result.gas_used();

        // append gas used
        self.inner.gas_used += gas_used;

        self.inner.receipts.push(
            match self.inner.receipt_builder.build_receipt(ReceiptBuilderCtx {
                tx: tx.inner(),
                result,
                cumulative_gas_used: self.inner.gas_used,
                evm: &self.inner.evm,
                state: &state,
            }) {
                Ok(receipt) => receipt,
                Err(ctx) => {
                    let receipt = alloy_consensus::Receipt {
                        // Success flag was added in `EIP-658: Embedding transaction status code
                        // in receipts`.
                        status: Eip658Value::Eip658(ctx.result.is_success()),
                        cumulative_gas_used: self.inner.gas_used,
                        logs: ctx.result.into_logs(),
                    };

                    self.inner
                        .receipt_builder
                        .build_deposit_receipt(OpDepositReceipt {
                            inner: receipt,
                            deposit_nonce: depositor.map(|account| account.nonce),
                            // The deposit receipt version was introduced in Canyon to indicate an
                            // update to how receipt hashes should be computed
                            // when set. The state transition process ensures
                            // this is only set for post-Canyon deposit
                            // transactions.
                            deposit_receipt_version: (is_deposit
                                && self.inner.spec.is_canyon_active_at_timestamp(
                                    self.inner.evm.block().timestamp,
                                ))
                            .then_some(1),
                        })
                }
            },
        );

        self.inner.evm.db_mut().commit(state);

        Ok(gas_used)
    }

    fn finish(self) -> Result<(Self::Evm, BlockExecutionResult<R::Receipt>), BlockExecutionError> {
        self.inner.finish()
    }

    fn set_state_hook(&mut self, _hook: Option<Box<dyn OnStateHook>>) {
        self.inner.set_state_hook(_hook)
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.inner.evm_mut()
    }
}
