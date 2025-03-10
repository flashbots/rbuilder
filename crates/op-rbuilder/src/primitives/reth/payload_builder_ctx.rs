use crate::metrics::OpRBuilderMetrics;
use crate::primitives::reth::ExecutionInfo;
#[cfg(not(feature = "flashblocks"))]
use crate::primitives::signed_builder_tx;
#[cfg(not(feature = "flashblocks"))]
use crate::tx_signer::Signer;
use alloy_consensus::{Eip658Value, Transaction};
use alloy_eips::eip4895::Withdrawals;
use alloy_eips::Typed2718;
#[cfg(not(feature = "flashblocks"))]
use alloy_primitives::private::alloy_rlp::Encodable;
use alloy_primitives::{Bytes, U256};
use alloy_rpc_types_engine::PayloadId;
use op_alloy_consensus::OpDepositReceipt;
use reth_basic_payload_builder::PayloadConfig;
use reth_chainspec::EthChainSpec;
use reth_evm::{
    env::EvmEnv, system_calls::SystemCaller, ConfigureEvmEnv, ConfigureEvmFor, Database, Evm,
    EvmError, InvalidTxError,
};
use reth_optimism_evm::{OpReceiptBuilder, ReceiptBuilderCtx};
use reth_optimism_forks::OpHardforks;
use reth_optimism_payload_builder::config::OpDAConfig;
use reth_optimism_payload_builder::error::OpPayloadBuilderError;
use reth_optimism_payload_builder::{OpPayloadBuilderAttributes, OpPayloadPrimitives};
use reth_optimism_primitives::OpTransactionSigned;
use reth_payload_primitives::{PayloadBuilderAttributes, PayloadBuilderError};
use reth_payload_util::PayloadTransactions;
use reth_primitives::{transaction::SignedTransactionIntoRecoveredExt, SealedHeader};
use reth_primitives_traits::{InMemorySize, NodePrimitives, SignedTransaction};
use reth_provider::ProviderError;
use reth_transaction_pool::{BestTransactionsAttributes, PoolTransaction};
use revm::{
    db::State,
    primitives::{ExecutionResult, ResultAndState},
    DatabaseCommit,
};
use std::error::Error as StdError;
use std::{fmt::Display, sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::{trace, warn};

/// Container type that holds all necessities to build a new payload.
#[derive(Debug)]
pub struct OpPayloadBuilderCtx<EvmConfig: ConfigureEvmEnv, ChainSpec, N: NodePrimitives> {
    /// The type that knows how to perform system calls and configure the evm.
    pub evm_config: EvmConfig,
    /// The DA config for the payload builder
    pub da_config: OpDAConfig,
    /// The chainspec
    pub chain_spec: Arc<ChainSpec>,
    /// How to build the payload.
    pub config: PayloadConfig<OpPayloadBuilderAttributes<N::SignedTx>>,
    /// Evm Settings
    pub evm_env: EvmEnv<EvmConfig::Spec>,
    /// Marker to check whether the job has been cancelled.
    pub cancel: CancellationToken,
    /// Receipt builder.
    pub receipt_builder: Arc<dyn OpReceiptBuilder<N::SignedTx, Receipt = N::Receipt>>,
    #[cfg(not(feature = "flashblocks"))]
    /// The builder signer
    pub builder_signer: Option<Signer>,
    /// The metrics for the builder
    pub metrics: OpRBuilderMetrics,
}

impl<EvmConfig, ChainSpec, N> OpPayloadBuilderCtx<EvmConfig, ChainSpec, N>
where
    EvmConfig: ConfigureEvmEnv,
    ChainSpec: EthChainSpec + OpHardforks,
    N: NodePrimitives,
{
    /// Returns the parent block the payload will be build on.
    pub fn parent(&self) -> &SealedHeader {
        &self.config.parent_header
    }

    /// Returns the builder attributes.
    pub const fn attributes(&self) -> &OpPayloadBuilderAttributes<N::SignedTx> {
        &self.config.attributes
    }

    /// Returns the withdrawals if shanghai is active.
    pub fn withdrawals(&self) -> Option<&Withdrawals> {
        self.chain_spec
            .is_shanghai_active_at_timestamp(self.attributes().timestamp())
            .then(|| &self.attributes().payload_attributes.withdrawals)
    }

    /// Returns the block gas limit to target.
    pub fn block_gas_limit(&self) -> u64 {
        self.attributes()
            .gas_limit
            .unwrap_or_else(|| self.evm_env.block_env.gas_limit.saturating_to())
    }

    /// Returns the block number for the block.
    pub fn block_number(&self) -> u64 {
        self.evm_env.block_env.number.to()
    }

    /// Returns the current base fee
    pub fn base_fee(&self) -> u64 {
        self.evm_env.block_env.basefee.to()
    }

    /// Returns the current blob gas price.
    pub fn get_blob_gasprice(&self) -> Option<u64> {
        self.evm_env
            .block_env
            .get_blob_gasprice()
            .map(|gasprice| gasprice as u64)
    }

    /// Returns the blob fields for the header.
    ///
    /// This will always return `Some(0)` after ecotone.
    pub fn blob_fields(&self) -> (Option<u64>, Option<u64>) {
        // OP doesn't support blobs/EIP-4844.
        // https://specs.optimism.io/protocol/exec-engine.html#ecotone-disable-blob-transactions
        // Need [Some] or [None] based on hardfork to match block hash.
        if self.is_ecotone_active() {
            (Some(0), Some(0))
        } else {
            (None, None)
        }
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
    pub fn best_transaction_attributes(&self) -> BestTransactionsAttributes {
        BestTransactionsAttributes::new(self.base_fee(), self.get_blob_gasprice())
    }

    /// Returns the unique id for this payload job.
    pub fn payload_id(&self) -> PayloadId {
        self.attributes().payload_id()
    }

    /// Returns true if regolith is active for the payload.
    pub fn is_regolith_active(&self) -> bool {
        self.chain_spec
            .is_regolith_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if ecotone is active for the payload.
    pub fn is_ecotone_active(&self) -> bool {
        self.chain_spec
            .is_ecotone_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if canyon is active for the payload.
    pub fn is_canyon_active(&self) -> bool {
        self.chain_spec
            .is_canyon_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if holocene is active for the payload.
    pub fn is_holocene_active(&self) -> bool {
        self.chain_spec
            .is_holocene_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if isthmus is active for the payload.
    pub fn is_isthmus_active(&self) -> bool {
        self.chain_spec
            .is_isthmus_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns the chain id
    pub fn chain_id(&self) -> u64 {
        self.chain_spec.chain_id()
    }

    #[cfg(not(feature = "flashblocks"))]
    /// Returns the builder signer
    pub fn builder_signer(&self) -> Option<Signer> {
        self.builder_signer
    }

    /// Ensure that the create2deployer is force-deployed at the canyon transition. Optimism
    /// blocks will always have at least a single transaction in them (the L1 info transaction),
    /// so we can safely assume that this will always be triggered upon the transition and that
    /// the above check for empty blocks will never be hit on OP chains.
    pub fn ensure_create2_deployer<DB>(&self, db: &mut State<DB>) -> Result<(), PayloadBuilderError>
    where
        DB: Database,
        DB::Error: Display,
    {
        reth_optimism_evm::ensure_create2_deployer(
            self.chain_spec.clone(),
            self.attributes().payload_attributes.timestamp,
            db,
        )
        .map_err(|err| {
            warn!(target: "payload_builder", %err, "missing create2 deployer, skipping block.");
            PayloadBuilderError::other(OpPayloadBuilderError::ForceCreate2DeployerFail)
        })
    }
}

impl<EvmConfig, ChainSpec, N> OpPayloadBuilderCtx<EvmConfig, ChainSpec, N>
where
    EvmConfig: ConfigureEvmFor<N>,
    ChainSpec: EthChainSpec + OpHardforks,
    N: OpPayloadPrimitives<_TX = OpTransactionSigned>,
{
    /// apply eip-4788 pre block contract call
    pub fn apply_pre_beacon_root_contract_call<DB>(
        &self,
        db: &mut DB,
    ) -> Result<(), PayloadBuilderError>
    where
        DB: Database + DatabaseCommit,
        DB::Error: Display,
        <DB as revm::Database>::Error: StdError,
    {
        SystemCaller::new(self.evm_config.clone(), self.chain_spec.clone())
            .pre_block_beacon_root_contract_call(
                db,
                &self.evm_env,
                self.attributes()
                    .payload_attributes
                    .parent_beacon_block_root,
            )
            .map_err(|err| {
                warn!(target: "payload_builder",
                    parent_header=%self.parent().hash(),
                    %err,
                    "failed to apply beacon root contract call for payload"
                );
                PayloadBuilderError::Internal(err.into())
            })?;

        Ok(())
    }

    /// Constructs a receipt for the given transaction.
    fn build_receipt(
        &self,
        info: &ExecutionInfo<N>,
        result: ExecutionResult,
        deposit_nonce: Option<u64>,
        tx: &N::SignedTx,
    ) -> N::Receipt {
        match self.receipt_builder.build_receipt(ReceiptBuilderCtx {
            tx,
            result,
            cumulative_gas_used: info.cumulative_gas_used,
        }) {
            Ok(receipt) => receipt,
            Err(ctx) => {
                let receipt = alloy_consensus::Receipt {
                    // Success flag was added in `EIP-658: Embedding transaction status code
                    // in receipts`.
                    status: Eip658Value::Eip658(ctx.result.is_success()),
                    cumulative_gas_used: ctx.cumulative_gas_used,
                    logs: ctx.result.into_logs(),
                };

                self.receipt_builder
                    .build_deposit_receipt(OpDepositReceipt {
                        inner: receipt,
                        deposit_nonce,
                        // The deposit receipt version was introduced in Canyon to indicate an
                        // update to how receipt hashes should be computed
                        // when set. The state transition process ensures
                        // this is only set for post-Canyon deposit
                        // transactions.
                        deposit_receipt_version: self.is_canyon_active().then_some(1),
                    })
            }
        }
    }

    /// Executes all sequencer transactions that are included in the payload attributes.
    pub fn execute_sequencer_transactions<DB>(
        &self,
        db: &mut State<DB>,
    ) -> Result<ExecutionInfo<N>, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError>,
    {
        let mut info = ExecutionInfo::with_capacity(self.attributes().transactions.len());

        let mut evm = self.evm_config.evm_with_env(&mut *db, self.evm_env.clone());

        for sequencer_tx in &self.attributes().transactions {
            // A sequencer's block should never contain blob transactions.
            if sequencer_tx.value().is_eip4844() {
                return Err(PayloadBuilderError::other(
                    OpPayloadBuilderError::BlobTransactionRejected,
                ));
            }

            // Convert the transaction to a [Recovered<TransactionSigned>]. This is
            // purely for the purposes of utilizing the `evm_config.tx_env`` function.
            // Deposit transactions do not have signatures, so if the tx is a deposit, this
            // will just pull in its `from` address.
            let sequencer_tx = sequencer_tx
                .value()
                .try_clone_into_recovered()
                .map_err(|_| {
                    PayloadBuilderError::other(OpPayloadBuilderError::TransactionEcRecoverFailed)
                })?;

            // Cache the depositor account prior to the state transition for the deposit nonce.
            //
            // Note that this *only* needs to be done post-regolith hardfork, as deposit nonces
            // were not introduced in Bedrock. In addition, regular transactions don't have deposit
            // nonces, so we don't need to touch the DB for those.
            let depositor_nonce = (self.is_regolith_active() && sequencer_tx.is_deposit())
                .then(|| {
                    evm.db_mut()
                        .load_cache_account(sequencer_tx.signer())
                        .map(|acc| acc.account_info().unwrap_or_default().nonce)
                })
                .transpose()
                .map_err(|_| {
                    PayloadBuilderError::other(OpPayloadBuilderError::AccountLoadFailed(
                        sequencer_tx.signer(),
                    ))
                })?;

            let tx_env = self
                .evm_config
                .tx_env(sequencer_tx.tx(), sequencer_tx.signer());

            let ResultAndState { result, state } = match evm.transact(tx_env) {
                Ok(res) => res,
                Err(err) => {
                    if err.is_invalid_tx_err() {
                        trace!(target: "payload_builder", %err, ?sequencer_tx, "Error in sequencer transaction, skipping.");
                        continue;
                    }
                    // this is an error that we should treat as fatal for this attempt
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            // commit changes
            evm.db_mut().commit(state);

            let gas_used = result.gas_used();

            // add gas used by the transaction to cumulative gas used, before creating the receipt
            info.cumulative_gas_used += gas_used;

            info.receipts.push(self.build_receipt(
                &info,
                result,
                depositor_nonce,
                sequencer_tx.tx(),
            ));

            // append sender and transaction to the respective lists
            info.executed_senders.push(sequencer_tx.signer());
            info.executed_transactions.push(sequencer_tx.into_tx());
        }

        Ok(info)
    }

    /// Executes the given best transactions and updates the execution info.
    ///
    /// Returns `Ok(Some(())` if the job was cancelled.
    pub fn execute_best_transactions<DB>(
        &self,
        info: &mut ExecutionInfo<N>,
        db: &mut State<DB>,
        mut best_txs: impl PayloadTransactions<
            Transaction: PoolTransaction<Consensus = EvmConfig::Transaction>,
        >,
        // In case of vanilla builder it's block gas limit, in case of flashblocks it's batch gas limit
        gas_limit: u64,
        block_da_limit: Option<u64>,
        tx_da_limit: Option<u64>,
    ) -> Result<Option<()>, PayloadBuilderError>
    where
        DB: Database<Error = ProviderError>,
    {
        let execute_txs_start_time = Instant::now();
        let mut num_txs_considered = 0;
        let mut num_txs_simulated = 0;
        let mut num_txs_simulated_success = 0;
        let mut num_txs_simulated_fail = 0;
        let base_fee = self.base_fee();
        let mut evm = self.evm_config.evm_with_env(&mut *db, self.evm_env.clone());

        while let Some(tx) = best_txs.next(()) {
            let tx = tx.into_consensus();

            // TODO: we have it here because flashblocks need this check
            // check in info if the txn has been executed already
            if info.executed_transactions.contains(&tx) {
                continue;
            }

            num_txs_considered += 1;
            // ensure we still have capacity for this transaction
            if info.is_tx_over_limits(tx.tx(), gas_limit, tx_da_limit, block_da_limit) {
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

            // check if the job was cancelled, if so we can exit early
            if self.cancel.is_cancelled() {
                return Ok(Some(()));
            }

            // Configure the environment for the tx.
            let tx_env = self.evm_config.tx_env(tx.tx(), tx.signer());

            let tx_simulation_start_time = Instant::now();

            let ResultAndState { result, state } = match evm.transact(tx_env) {
                Ok(res) => res,
                Err(err) => {
                    if let Some(err) = err.as_invalid_tx_err() {
                        if err.is_nonce_too_low() {
                            // if the nonce is too low, we can skip this transaction
                            trace!(target: "payload_builder", %err, ?tx, "skipping nonce too low transaction");
                        } else {
                            // if the transaction is invalid, we can skip it and all of its
                            // descendants
                            trace!(target: "payload_builder", %err, ?tx, "skipping invalid transaction and its descendants");
                            best_txs.mark_invalid(tx.signer(), tx.nonce());
                        }

                        continue;
                    }
                    // this is an error that we should treat as fatal for this attempt
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            self.metrics
                .tx_simulation_duration
                .record(tx_simulation_start_time.elapsed());
            self.metrics.tx_byte_size.record(tx.tx().size() as f64);
            num_txs_simulated += 1;
            if result.is_success() {
                num_txs_simulated_success += 1;
            } else {
                num_txs_simulated_fail += 1;
                trace!(target: "payload_builder", ?tx, "skipping reverted transaction");
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                info.reverted_tx_hashes.insert(*tx.tx_hash());
                continue;
            }

            // commit changes
            evm.db_mut().commit(state);

            let gas_used = result.gas_used();

            // add gas used by the transaction to cumulative gas used, before creating the
            // receipt
            info.cumulative_gas_used += gas_used;

            // Push transaction changeset and calculate header bloom filter for receipt.
            info.receipts
                .push(self.build_receipt(info, result, None, &tx));

            // update add to total fees
            let miner_fee = tx
                .effective_tip_per_gas(base_fee)
                .expect("fee is always valid; execution succeeded");
            info.total_fees += U256::from(miner_fee) * U256::from(gas_used);

            // append sender and transaction to the respective lists
            info.executed_senders.push(tx.signer());
            info.executed_transactions.push(tx.into_tx());
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

    #[cfg(not(feature = "flashblocks"))]
    pub fn add_builder_tx<DB>(
        &self,
        info: &mut ExecutionInfo<N>,
        db: &mut State<DB>,
        builder_tx_gas: u64,
        message: Vec<u8>,
    ) -> Option<()>
    where
        DB: Database<Error = ProviderError>,
    {
        self.builder_signer()
            .map(|signer| {
                let base_fee = self.base_fee();
                let chain_id = self.chain_id();
                // Create and sign the transaction
                let builder_tx =
                    signed_builder_tx(db, builder_tx_gas, message, signer, base_fee, chain_id)?;

                let mut evm = self.evm_config.evm_with_env(&mut *db, self.evm_env.clone());
                let tx_env = self.evm_config.tx_env(builder_tx.tx(), builder_tx.signer());

                let ResultAndState { result, state } = evm
                    .transact(tx_env)
                    .map_err(|err| PayloadBuilderError::EvmExecutionError(Box::new(err)))?;

                // Release the db reference by dropping evm
                drop(evm);
                // Commit changes
                db.commit(state);

                let gas_used = result.gas_used();

                // Add gas used by the transaction to cumulative gas used, before creating the receipt
                info.cumulative_gas_used += gas_used;

                info.receipts
                    .push(self.build_receipt(info, result, None, &builder_tx));

                // Append sender and transaction to the respective lists
                info.executed_senders.push(builder_tx.signer());
                info.executed_transactions.push(builder_tx.into_tx());
                Ok(())
            })
            .transpose()
            .unwrap_or_else(|err: PayloadBuilderError| {
                warn!(target: "payload_builder", %err, "Failed to add builder transaction");
                None
            })
    }

    #[cfg(not(feature = "flashblocks"))]
    /// Calculates EIP 2718 builder transaction size
    pub fn estimate_builder_tx_da_size<DB>(
        &self,
        db: &mut State<DB>,
        builder_tx_gas: u64,
        message: Vec<u8>,
    ) -> Option<usize>
    where
        DB: Database<Error = ProviderError>,
    {
        self.builder_signer()
            .map(|signer| {
                let base_fee = self.base_fee();
                let chain_id = self.chain_id();
                // Create and sign the transaction
                let builder_tx =
                    signed_builder_tx(db, builder_tx_gas, message, signer, base_fee, chain_id)?;
                Ok(builder_tx.length())
            })
            .transpose()
            .unwrap_or_else(|err: PayloadBuilderError| {
                warn!(target: "payload_builder", %err, "Failed to add builder transaction");
                None
            })
    }
}
