use super::{
    create_payout_tx, tracers::SimulationTracer, BlockBuildingContext, EstimatePayoutGasErr,
};
use crate::{
    building::estimate_payout_gas_limit,
    primitives::{
        Bundle, Order, OrderId, RefundConfig, ShareBundle, ShareBundleBody, ShareBundleInner,
        TransactionSignedEcRecoveredWithBlobs,
    },
    utils::get_percent,
};

use alloy_primitives::{Address, B256, U256};

use reth::revm::database::StateProviderDatabase;
use reth_errors::ProviderError;
use reth_payload_builder::database::CachedReads;
use reth_primitives::{
    constants::eip4844::{DATA_GAS_PER_BLOB, MAX_DATA_GAS_PER_BLOCK},
    transaction::FillTxEnv,
    Receipt, KECCAK_EMPTY,
};
use reth_provider::{StateProvider, StateProviderBox};
use revm::{
    db::{states::bundle_state::BundleRetention, BundleState},
    inspector_handle_register,
    primitives::{db::WrapDatabaseRef, EVMError, Env, ExecutionResult, InvalidTransaction, TxEnv},
    Database, DatabaseCommit, State,
};

use crate::building::evm_inspector::{RBuilderEVMInspector, UsedStateTrace};
use std::{collections::HashMap, sync::Arc};
use thiserror::Error;

#[derive(Clone)]
pub struct BlockState {
    provider: Arc<dyn StateProvider>,
    cached_reads: CachedReads,
    bundle_state: Option<BundleState>,
}

impl BlockState {
    pub fn new(provider: StateProviderBox) -> Self {
        Self::new_arc(Arc::from(provider))
    }

    pub fn new_arc(provider: Arc<dyn StateProvider>) -> Self {
        Self {
            provider,
            cached_reads: CachedReads::default(),
            bundle_state: Some(BundleState::default()),
        }
    }

    pub fn state_provider(&self) -> &dyn StateProvider {
        &self.provider
    }

    pub fn into_provider(self) -> Arc<dyn StateProvider> {
        self.provider
    }

    pub fn with_cached_reads(mut self, cached_reads: CachedReads) -> Self {
        self.cached_reads = cached_reads;
        self
    }

    pub fn with_bundle_state(mut self, bundle_state: BundleState) -> Self {
        self.bundle_state = Some(bundle_state);
        self
    }

    pub fn into_parts(self) -> (CachedReads, BundleState, Arc<dyn StateProvider>) {
        (self.cached_reads, self.bundle_state.unwrap(), self.provider)
    }

    pub fn clone_bundle_and_cache(&self) -> (CachedReads, BundleState) {
        (
            self.cached_reads.clone(),
            self.bundle_state.clone().unwrap(),
        )
    }

    pub fn new_db_ref(&mut self) -> BlockStateDBRef<impl Database<Error = ProviderError> + '_> {
        let state_provider = StateProviderDatabase::new(&self.provider);
        let cachedb = WrapDatabaseRef(self.cached_reads.as_db(state_provider));
        let bundle_state = self.bundle_state.take().unwrap();
        let db = State::builder()
            .with_database(cachedb)
            .with_bundle_prestate(bundle_state)
            .with_bundle_update()
            .build();
        BlockStateDBRef::new(db, &mut self.bundle_state)
    }

    pub fn balance(&mut self, address: Address) -> Result<U256, ProviderError> {
        let mut db = self.new_db_ref();
        Ok(db
            .as_mut()
            .basic(address)?
            .map(|acc| acc.balance)
            .unwrap_or_default())
    }

    pub fn nonce(&mut self, address: Address) -> Result<u64, ProviderError> {
        let mut db = self.new_db_ref();
        Ok(db
            .as_mut()
            .basic(address)?
            .map(|acc| acc.nonce)
            .unwrap_or_default())
    }

    pub fn code_hash(&mut self, address: Address) -> Result<B256, ProviderError> {
        let mut db = self.new_db_ref();
        Ok(db
            .as_mut()
            .basic(address)?
            .map(|acc| acc.code_hash)
            .unwrap_or_else(|| KECCAK_EMPTY))
    }

    pub fn clone_cached_reads(&self) -> CachedReads {
        self.cached_reads.clone()
    }
}

/// A wrapper around a [`State`] that will return the [`BundleState`] back to [`BlockState`] when dropped.
pub struct BlockStateDBRef<'a, DB>
where
    DB: Database<Error = ProviderError>,
{
    db: State<DB>,
    parent_bundle_state_ref: &'a mut Option<BundleState>,
}

impl<'a, DB> BlockStateDBRef<'a, DB>
where
    DB: Database<Error = ProviderError>,
{
    pub fn new(db: State<DB>, parent_bundle_state_ref: &'a mut Option<BundleState>) -> Self {
        Self {
            db,
            parent_bundle_state_ref,
        }
    }

    pub fn db(&mut self) -> &mut State<DB> {
        &mut self.db
    }
}

impl<'a, DB> Drop for BlockStateDBRef<'a, DB>
where
    DB: Database<Error = ProviderError>,
{
    fn drop(&mut self) {
        *self.parent_bundle_state_ref = Some(self.db.take_bundle())
    }
}

impl<'a, DB> AsRef<State<DB>> for BlockStateDBRef<'a, DB>
where
    DB: Database<Error = ProviderError>,
{
    fn as_ref(&self) -> &State<DB> {
        &self.db
    }
}

impl<'a, DB> AsMut<State<DB>> for BlockStateDBRef<'a, DB>
where
    DB: Database<Error = ProviderError>,
{
    fn as_mut(&mut self) -> &mut State<DB> {
        &mut self.db
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionOk {
    pub exec_result: ExecutionResult,
    pub gas_used: u64,
    pub cumulative_gas_used: u64,
    pub blob_gas_used: u64,
    pub cumulative_blob_gas_used: u64,
    pub tx: TransactionSignedEcRecoveredWithBlobs,
    /// nonces_updates is nonce after tx was applied.
    /// account nonce was 0, tx was included, nonce is 1. => nonce_updated.1 == 1
    pub nonce_updated: (Address, u64),
    pub receipt: Receipt,
}

#[derive(Error, Debug, Eq, PartialEq)]
pub enum TransactionErr {
    #[error("Invalid transaction: {0:?}")]
    InvalidTransaction(InvalidTransaction),
    #[error("Blocklist violation error")]
    Blocklist,
    #[error("Gas left is too low")]
    GasLeft,
    #[error("Blob Gas left is too low")]
    BlobGasLeft,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleOk {
    pub gas_used: u64,
    pub cumulative_gas_used: u64,
    pub blob_gas_used: u64,
    pub cumulative_blob_gas_used: u64,
    pub txs: Vec<TransactionSignedEcRecoveredWithBlobs>,
    /// nonces_updates has a set of deduplicated final nonces of the txs in the order
    pub nonces_updated: Vec<(Address, u64)>,
    pub receipts: Vec<Receipt>,
    pub paid_kickbacks: Vec<(Address, U256)>,
    /// Only for sbundles we accumulate ShareBundleInner::original_order_id that executed ok.
    /// Its original use is for only one level or orders with original_order_id but if nesting happens the parent order original_order_id goes before its children (pre-order DFS)
    /// Fully dropped orders (TxRevertBehavior::AllowedExcluded allows it!) are not included.
    pub original_order_ids: Vec<OrderId>,
}

#[derive(Error, Debug, Eq, PartialEq)]
pub enum BundleErr {
    #[error("Invalid transaction, hash: {0:?}, err: {1}")]
    InvalidTransaction(B256, TransactionErr),
    #[error("Transaction reverted: {0:?}")]
    TransactionReverted(B256),
    #[error("Bundle inserted empty")]
    EmptyBundle,
    #[error(
        "Trying to commit bundle for incorrect block, block: {block}, target_blocks: {target_block}-{target_max_block}"
    )]
    TargetBlockIncorrect {
        block: u64,
        target_block: u64,
        target_max_block: u64,
    },
    #[error("Not enough refund for gas, to: {to:?}, refundable_value: {refundable_value}, needed_value: {needed_value}")]
    NotEnoughRefundForGas {
        to: Address,
        refundable_value: U256,
        needed_value: U256,
    },
    #[error(
        "Failed to commit payout tx, to: {to:?}, gas_limit: {gas_limit}, value: {value}, err: {err:?}"
    )]
    FailedToCommitPayoutTx {
        to: Address,
        gas_limit: u64,
        value: U256,
        // if none, tx just reverted
        err: Option<TransactionErr>,
    },
    #[error("Failed to estimate payout gas: {0}")]
    EstimatePayoutGas(#[from] EstimatePayoutGasErr),
    #[error("Failed to create payout tx: {0}")]
    PayoutTx(#[from] secp256k1::Error),
    #[error("Incorrect refundable element: {0}")]
    IncorrectRefundableElement(usize),
    #[error("Incorrect timestamp, min: {min}, max: {max}, block: {block}")]
    IncorrectTimestamp { min: u64, max: u64, block: u64 },
    #[error("Mev-share without signer")]
    NoSigner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderOk {
    pub coinbase_profit: U256,
    pub gas_used: u64,
    pub cumulative_gas_used: u64,
    pub blob_gas_used: u64,
    pub cumulative_blob_gas_used: u64,
    pub txs: Vec<TransactionSignedEcRecoveredWithBlobs>,
    /// Patch to get the executed OrderIds for merged sbundles (see: [`BundleOk::original_order_ids`],[`ShareBundleMerger`] )
    pub original_order_ids: Vec<OrderId>,
    /// nonces_updates has a set of deduplicated final nonces of the txs in the order
    pub nonces_updated: Vec<(Address, u64)>,
    pub receipts: Vec<Receipt>,
    pub paid_kickbacks: Vec<(Address, U256)>,
    pub used_state_trace: Option<UsedStateTrace>,
}

#[derive(Error, Debug, Eq, PartialEq)]
pub enum OrderErr {
    #[error("Transaction error: {0}")]
    Transaction(#[from] TransactionErr),
    #[error("Bundle error: {0}")]
    Bundle(#[from] BundleErr),
    #[error("Negative profit: {0}")]
    NegativeProfit(U256),
}

pub struct PartialBlockFork<'a, 'b, Tracer: SimulationTracer> {
    pub rollbacks: usize,
    pub state: &'a mut BlockState,
    pub tracer: Option<&'b mut Tracer>,
}

pub struct PartialBlockRollobackPoint {
    rollobacks: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservedPayout {
    pub gas_limit: u64,
    pub tx_value: U256,
    pub total_refundable_value: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareBundleCommitResult {
    pub bundle_ok: BundleOk,
    pub coinbase_diff_before_payouts: U256,
    pub total_payouts_promissed: U256,
    pub payouts_promissed: HashMap<Address, ReservedPayout>,
}

#[derive(thiserror::Error, Debug)]
pub enum CriticalCommitOrderError {
    #[error("Reth error: {0}")]
    Reth(#[from] ProviderError),
    #[error("EVM error: {0}")]
    EVM(#[from] EVMError<ProviderError>),
}

/// For all funcs allow_tx_skip means:
/// If a tx inside a bundle or sbundle fails with TransactionErr (don't confuse this with reverting which is TransactionOk with !.receipt.success)
/// and it's configured as allowed to revert (for bundles tx in reverting_tx_hashes, for sbundles: TxRevertBehavior != NotAllowed) we continue the
/// the execution of the bundle/sbundle.
impl<'a, 'b, Tracer: SimulationTracer> PartialBlockFork<'a, 'b, Tracer> {
    pub fn with_tracer<NewTracer: SimulationTracer>(
        self,
        tracer: &'b mut NewTracer,
    ) -> PartialBlockFork<'a, 'b, NewTracer> {
        PartialBlockFork {
            rollbacks: self.rollbacks,
            state: self.state,
            tracer: Some(tracer),
        }
    }

    pub fn rollback_point(&self) -> PartialBlockRollobackPoint {
        PartialBlockRollobackPoint {
            rollobacks: self.rollbacks,
        }
    }

    pub fn rollback(&mut self, rollback_point: PartialBlockRollobackPoint) {
        let rollbacks = self
            .rollbacks
            .checked_sub(rollback_point.rollobacks)
            .expect("incorrect rollback");
        let bundle_state = self.state.bundle_state.as_mut().expect("no bundle state");
        bundle_state.revert(rollbacks);
        self.rollbacks = rollback_point.rollobacks;
    }

    /// Helper func that executes f and rollbacks on Ok(Err).
    /// For CriticalCommitOrderError we don't rollback since it's a critical unrecoverable failure
    /// Use like this:
    /// self.execute_with_rollback(|s| {
    ///   s.commit or whatever
    /// })
    /// f needs to receive self to avoid double &mut
    /// Might be implemented nicer with macros.
    fn execute_with_rollback<
        OkType,
        ErrType,
        F: FnOnce(&mut Self) -> Result<Result<OkType, ErrType>, CriticalCommitOrderError>,
    >(
        &mut self,
        f: F,
    ) -> Result<Result<OkType, ErrType>, CriticalCommitOrderError> {
        let rollback_point = self.rollback_point();
        let res = f(self)?;
        if res.is_err() {
            self.rollback(rollback_point);
        }
        Ok(res)
    }

    /// The state is updated ONLY when we return Ok(Ok)
    pub fn commit_tx(
        &mut self,
        tx_with_blobs: &TransactionSignedEcRecoveredWithBlobs,
        ctx: &BlockBuildingContext,
        mut cumulative_gas_used: u64,
        gas_reserved: u64,
        mut cumulative_blob_gas_used: u64,
    ) -> Result<Result<TransactionOk, TransactionErr>, CriticalCommitOrderError> {
        // Use blobs.len() instead of checking for tx type just in case in the future some other new txs have blobs
        let blob_gas_used = tx_with_blobs.blobs_sidecar.blobs.len() as u64 * DATA_GAS_PER_BLOB;
        if cumulative_blob_gas_used + blob_gas_used > MAX_DATA_GAS_PER_BLOCK {
            return Ok(Err(TransactionErr::BlobGasLeft));
        }

        let mut db = self.state.new_db_ref();
        let tx = &tx_with_blobs.internal_tx_unsecure();
        if ctx.blocklist.contains(&tx.signer())
            || tx
                .to()
                .map(|to| ctx.blocklist.contains(&to))
                .unwrap_or(false)
        {
            return Ok(Err(TransactionErr::Blocklist));
        }

        match ctx
            .block_env
            .gas_limit
            .checked_sub(U256::from(cumulative_gas_used + gas_reserved))
        {
            Some(gas_left) => {
                if tx.gas_limit() > gas_left.to::<u64>() {
                    return Ok(Err(TransactionErr::GasLeft));
                }
            }
            None => return Ok(Err(TransactionErr::GasLeft)),
        }

        let mut tx_env = TxEnv::default();
        let tx_signed = tx_with_blobs.internal_tx_unsecure();
        tx_signed.fill_tx_env(&mut tx_env, tx_signed.signer());

        let env = Env {
            cfg: ctx.initialized_cfg.cfg_env.clone(),
            block: ctx.block_env.clone(),
            tx: tx_env,
        };

        let used_state_tracer = self.tracer.as_mut().and_then(|t| t.get_used_state_tracer());
        let mut rbuilder_inspector = RBuilderEVMInspector::new(tx, used_state_tracer);

        let mut evm = revm::Evm::builder()
            .with_spec_id(ctx.spec_id)
            .with_env(Box::new(env))
            .with_db(db.as_mut())
            .with_external_context(&mut rbuilder_inspector)
            .append_handler_register(inspector_handle_register)
            .build();
        let res = match evm.transact() {
            Ok(res) => res,
            Err(err) => match err {
                EVMError::Transaction(tx_err) => {
                    return Ok(Err(TransactionErr::InvalidTransaction(tx_err)))
                }
                EVMError::Database(_)
                | EVMError::Header(_)
                | EVMError::Custom(_)
                | EVMError::Precompile(_) => return Err(err.into()),
            },
        };
        let mut db_context = evm.into_context();
        let db = &mut db_context.evm.db;
        let access_list = rbuilder_inspector.into_access_list();
        if let Some(tracer) = &mut self.tracer {
            tracer.gas_used(res.result.gas_used());
        }
        if access_list
            .flatten()
            .any(|(a, _)| ctx.blocklist.contains(&a))
        {
            return Ok(Err(TransactionErr::Blocklist));
        }
        db.commit(res.state);
        db.merge_transitions(BundleRetention::Reverts);
        self.rollbacks += 1;

        // add gas used by the transaction to cumulative gas used, before creating the receipt
        let gas_used = res.result.gas_used();

        cumulative_gas_used += gas_used;
        cumulative_blob_gas_used += blob_gas_used;

        let receipt = Receipt {
            tx_type: tx.tx_type(),
            success: res.result.is_success(),
            cumulative_gas_used,
            logs: res.result.logs().to_vec(),
        };

        Ok(Ok(TransactionOk {
            exec_result: res.result,
            gas_used,
            blob_gas_used,
            cumulative_blob_gas_used,
            cumulative_gas_used,
            tx: tx_with_blobs.clone(),
            nonce_updated: (tx.signer(), tx.nonce() + 1),
            receipt,
        }))
    }

    /// block/timestamps check + commit_bundle_no_rollback + rollbacks
    fn commit_bundle(
        &mut self,
        bundle: &Bundle,
        ctx: &BlockBuildingContext,
        cumulative_gas_used: u64,
        gas_reserved: u64,
        cumulative_blob_gas_used: u64,
        allow_tx_skip: bool,
    ) -> Result<Result<BundleOk, BundleErr>, CriticalCommitOrderError> {
        let current_block = ctx.block_env.number.to::<u64>();
        if bundle.block != current_block {
            return Ok(Err(BundleErr::TargetBlockIncorrect {
                block: current_block,
                target_block: bundle.block,
                target_max_block: bundle.block,
            }));
        }

        let (min_ts, max_ts, block_ts) = (
            bundle.min_timestamp.unwrap_or(0),
            bundle.max_timestamp.unwrap_or(u64::MAX),
            ctx.block_env.timestamp.to::<u64>(),
        );
        if !(min_ts <= block_ts && block_ts <= max_ts) {
            return Ok(Err(BundleErr::IncorrectTimestamp {
                min: min_ts,
                max: max_ts,
                block: block_ts,
            }));
        }

        self.execute_with_rollback(|s| {
            s.commit_bundle_no_rollback(
                bundle,
                ctx,
                cumulative_gas_used,
                gas_reserved,
                cumulative_blob_gas_used,
                allow_tx_skip,
            )
        })
    }

    fn commit_bundle_no_rollback(
        &mut self,
        bundle: &Bundle,
        ctx: &BlockBuildingContext,
        cumulative_gas_used: u64,
        gas_reserved: u64,
        cumulative_blob_gas_used: u64,
        allow_tx_skip: bool,
    ) -> Result<Result<BundleOk, BundleErr>, CriticalCommitOrderError> {
        let mut insert = BundleOk {
            gas_used: 0,
            cumulative_gas_used,
            blob_gas_used: 0,
            cumulative_blob_gas_used,
            txs: Vec::new(),
            nonces_updated: Vec::new(),
            receipts: Vec::new(),
            paid_kickbacks: Vec::new(),
            original_order_ids: Vec::new(),
        };
        for tx_with_blobs in &bundle.txs {
            let result = self.commit_tx(
                tx_with_blobs,
                ctx,
                insert.cumulative_gas_used,
                gas_reserved,
                insert.cumulative_blob_gas_used,
            )?;
            match result {
                Ok(res) => {
                    if !res.receipt.success
                        && !bundle.reverting_tx_hashes.contains(&tx_with_blobs.hash())
                    {
                        return Ok(Err(BundleErr::TransactionReverted(tx_with_blobs.hash())));
                    }

                    insert.gas_used += res.gas_used;
                    insert.cumulative_gas_used = res.cumulative_gas_used;
                    insert.blob_gas_used += res.blob_gas_used;
                    insert.cumulative_blob_gas_used = res.cumulative_blob_gas_used;
                    insert.txs.push(res.tx);
                    update_nonce_list(&mut insert.nonces_updated, res.nonce_updated);
                    insert.receipts.push(res.receipt);
                }
                Err(err) => {
                    // if optional transaction, skip
                    if allow_tx_skip && bundle.reverting_tx_hashes.contains(&tx_with_blobs.hash()) {
                        continue;
                    } else {
                        return Ok(Err(BundleErr::InvalidTransaction(
                            tx_with_blobs.hash(),
                            err,
                        )));
                    }
                }
            }
        }
        if insert.gas_used == 0 {
            return Ok(Err(BundleErr::EmptyBundle));
        }
        Ok(Ok(insert))
    }

    /// block check + commit_share_bundle_no_rollback + rollback
    fn commit_share_bundle(
        &mut self,
        bundle: &ShareBundle,
        ctx: &BlockBuildingContext,
        cumulative_gas_used: u64,
        gas_reserved: u64,
        cumulative_blob_gas_used: u64,
        allow_tx_skip: bool,
    ) -> Result<Result<BundleOk, BundleErr>, CriticalCommitOrderError> {
        let current_block = ctx.block_env.number.to::<u64>();
        if !(bundle.block <= current_block && current_block <= bundle.max_block) {
            return Ok(Err(BundleErr::TargetBlockIncorrect {
                block: current_block,
                target_block: bundle.block,
                target_max_block: bundle.max_block,
            }));
        }
        self.execute_with_rollback(|s| {
            s.commit_share_bundle_no_rollback(
                bundle,
                ctx,
                cumulative_gas_used,
                gas_reserved,
                cumulative_blob_gas_used,
                allow_tx_skip,
            )
        })
    }

    /// Calls commit_share_bundle_inner to do all the hard work and, if everting goes ok, pays kickbacks
    fn commit_share_bundle_no_rollback(
        &mut self,
        bundle: &ShareBundle,
        ctx: &BlockBuildingContext,
        cumulative_gas_used: u64,
        gas_reserved: u64,
        cumulative_blob_gas_used: u64,
        allow_tx_skip: bool,
    ) -> Result<Result<BundleOk, BundleErr>, CriticalCommitOrderError> {
        let res = self.commit_share_bundle_inner(
            &bundle.inner_bundle,
            ctx,
            cumulative_gas_used,
            gas_reserved,
            cumulative_blob_gas_used,
            allow_tx_skip,
        )?;
        let res = match res {
            Ok(r) => r,
            Err(e) => {
                return Ok(Err(e));
            }
        };

        let mut insert = res.bundle_ok;

        // now pay all kickbacks
        for (
            to,
            ReservedPayout {
                gas_limit,
                tx_value: value,
                ..
            },
        ) in res.payouts_promissed.into_iter()
        {
            let builder_signer = if let Some(signer) = ctx.builder_signer.as_ref() {
                signer
            } else {
                return Ok(Err(BundleErr::NoSigner));
            };

            let nonce = self.state.nonce(builder_signer.address)?;
            let payout_tx = match create_payout_tx(
                ctx.chain_spec.as_ref(),
                ctx.block_env.basefee,
                builder_signer,
                nonce,
                to,
                gas_limit,
                value.to(),
            ) {
                // payout tx has no blobs so it's safe to unwrap
                Ok(tx) => TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx).unwrap(),
                Err(err) => {
                    return Ok(Err(BundleErr::PayoutTx(err)));
                }
            };
            let res = self.commit_tx(
                &payout_tx,
                ctx,
                insert.cumulative_gas_used,
                gas_reserved,
                insert.cumulative_blob_gas_used,
            )?;
            match res {
                Ok(res) => {
                    if !res.receipt.success {
                        return Ok(Err(BundleErr::FailedToCommitPayoutTx {
                            to,
                            gas_limit,
                            value,
                            err: None,
                        }));
                    }

                    insert.gas_used += res.gas_used;
                    insert.cumulative_gas_used = res.cumulative_gas_used;
                    insert.cumulative_blob_gas_used = res.cumulative_blob_gas_used;
                    insert.txs.push(res.tx);
                    update_nonce_list(&mut insert.nonces_updated, res.nonce_updated);
                    insert.receipts.push(res.receipt);
                    insert.paid_kickbacks.push((to, value));
                }
                Err(err) => {
                    return Ok(Err(BundleErr::FailedToCommitPayoutTx {
                        to,
                        gas_limit,
                        value,
                        err: Some(err),
                    }));
                }
            };
        }

        Ok(Ok(insert))
    }

    /// Only changes the state on Ok(Ok)
    fn commit_share_bundle_inner(
        &mut self,
        bundle: &ShareBundleInner,
        ctx: &BlockBuildingContext,
        cumulative_gas_used: u64,
        gas_reserved: u64,
        cumulative_blob_gas_used: u64,
        allow_tx_skip: bool,
    ) -> Result<Result<ShareBundleCommitResult, BundleErr>, CriticalCommitOrderError> {
        self.execute_with_rollback(|s| {
            s.commit_share_bundle_inner_no_rollback(
                bundle,
                ctx,
                cumulative_gas_used,
                gas_reserved,
                cumulative_blob_gas_used,
                allow_tx_skip,
            )
        })
    }

    fn commit_share_bundle_inner_no_rollback(
        &mut self,
        bundle: &ShareBundleInner,
        ctx: &BlockBuildingContext,
        cumulative_gas_used: u64,
        gas_reserved: u64,
        cumulative_blob_gas_used: u64,
        allow_tx_skip: bool,
    ) -> Result<Result<ShareBundleCommitResult, BundleErr>, CriticalCommitOrderError> {
        let mut insert = BundleOk {
            gas_used: 0,
            cumulative_gas_used,
            blob_gas_used: 0,
            cumulative_blob_gas_used,
            txs: Vec::new(),
            nonces_updated: Vec::new(),
            receipts: Vec::new(),
            paid_kickbacks: Vec::new(),
            original_order_ids: Vec::new(),
        };
        let coinbase_balance_before = self.state.balance(ctx.block_env.coinbase)?;
        let refundable_elements = bundle
            .refund
            .iter()
            .map(|r| (r.body_idx, r.percent))
            .collect::<HashMap<_, _>>();
        let mut refundable_profit = U256::from(0);
        let mut inner_payouts = HashMap::new();
        for (idx, body) in bundle.body.iter().enumerate() {
            match body {
                ShareBundleBody::Tx(sbundle_tx) => {
                    let rollback_point = self.rollback_point();
                    let tx = &sbundle_tx.tx;
                    let coinbase_balance_before = self.state.balance(ctx.block_env.coinbase)?;
                    let result = self.commit_tx(
                        tx,
                        ctx,
                        insert.cumulative_gas_used,
                        gas_reserved,
                        insert.cumulative_blob_gas_used,
                    )?;
                    match result {
                        Ok(res) => {
                            if !res.receipt.success {
                                match sbundle_tx.revert_behavior {
                                    crate::primitives::TxRevertBehavior::NotAllowed => {
                                        return Ok(Err(BundleErr::TransactionReverted(tx.hash())));
                                    }
                                    crate::primitives::TxRevertBehavior::AllowedIncluded => {}
                                    crate::primitives::TxRevertBehavior::AllowedExcluded => {
                                        self.rollback(rollback_point);
                                        continue;
                                    }
                                }
                            }

                            let coinbase_profit = {
                                let coinbase_balance_after =
                                    self.state.balance(ctx.block_env.coinbase)?;
                                coinbase_balance_after.checked_sub(coinbase_balance_before)
                            };
                            if let Some(profit) = coinbase_profit {
                                if !refundable_elements.contains_key(&idx) {
                                    refundable_profit += profit;
                                }
                            }

                            insert.gas_used += res.gas_used;
                            insert.cumulative_gas_used = res.cumulative_gas_used;
                            insert.blob_gas_used += res.blob_gas_used;
                            insert.cumulative_blob_gas_used = res.cumulative_blob_gas_used;
                            insert.txs.push(res.tx);
                            update_nonce_list(&mut insert.nonces_updated, res.nonce_updated);
                            insert.receipts.push(res.receipt);
                        }
                        Err(err) => {
                            // if optional transaction, skip
                            if allow_tx_skip && sbundle_tx.revert_behavior.can_revert() {
                                continue;
                            } else {
                                return Ok(Err(BundleErr::InvalidTransaction(tx.hash(), err)));
                            }
                        }
                    }
                }
                ShareBundleBody::Bundle(inner_bundle) => {
                    let inner_res = self.commit_share_bundle_inner(
                        inner_bundle,
                        ctx,
                        insert.cumulative_gas_used,
                        gas_reserved,
                        insert.cumulative_blob_gas_used,
                        allow_tx_skip,
                    )?;
                    match inner_res {
                        Ok(res) => {
                            if let Some(original_order_id) = inner_bundle.original_order_id {
                                if !res.bundle_ok.txs.is_empty() {
                                    // We only consider this order executed if something was so we exclude 100% dropped bundles.
                                    insert.original_order_ids.push(original_order_id);
                                }
                            }
                            if res.coinbase_diff_before_payouts > res.total_payouts_promissed
                                && !refundable_elements.contains_key(&idx)
                            {
                                refundable_profit +=
                                    res.coinbase_diff_before_payouts - res.total_payouts_promissed
                            }
                            insert
                                .original_order_ids
                                .extend(res.bundle_ok.original_order_ids);
                            insert.gas_used += res.bundle_ok.gas_used;
                            insert.cumulative_gas_used = res.bundle_ok.cumulative_gas_used;
                            insert.blob_gas_used += res.bundle_ok.blob_gas_used;
                            insert.cumulative_blob_gas_used =
                                res.bundle_ok.cumulative_blob_gas_used;
                            insert.txs.extend(res.bundle_ok.txs);
                            update_nonce_list_with_updates(
                                &mut insert.nonces_updated,
                                res.bundle_ok.nonces_updated,
                            );
                            insert.receipts.extend(res.bundle_ok.receipts);

                            for (addr, reserve) in res.payouts_promissed {
                                inner_payouts
                                    .entry(addr)
                                    .and_modify(|v| {
                                        *v += reserve.total_refundable_value;
                                    })
                                    .or_insert(reserve.total_refundable_value);
                            }
                        }
                        Err(err) => {
                            if inner_bundle.can_skip {
                                continue;
                            } else {
                                return Ok(Err(err));
                            }
                        }
                    }
                }
            }
        }

        for (idx, percent) in refundable_elements {
            let refund_config =
                if let Some(config) = bundle.body.get(idx).and_then(|b| b.refund_config()) {
                    config
                } else {
                    return Ok(Err(BundleErr::IncorrectRefundableElement(idx)));
                };

            let total_value = get_percent(refundable_profit, percent);
            for RefundConfig { address, percent } in refund_config {
                let value = get_percent(total_value, percent);
                inner_payouts
                    .entry(address)
                    .and_modify(|v| {
                        *v += value;
                    })
                    .or_insert(value);
            }
        }

        // calculate gas limits
        let mut payouts_promised = HashMap::new();
        for (to, refundable_value) in inner_payouts.drain() {
            let gas_limit =
                match estimate_payout_gas_limit(to, ctx, self.state, insert.cumulative_gas_used) {
                    Ok(gas_limit) => gas_limit,
                    Err(err) => {
                        return Ok(Err(BundleErr::EstimatePayoutGas(err)));
                    }
                };
            let base_fee = ctx.block_env.basefee * U256::from(gas_limit);
            if base_fee > refundable_value {
                return Ok(Err(BundleErr::NotEnoughRefundForGas {
                    to,
                    refundable_value,
                    needed_value: base_fee,
                }));
            }
            let tx_value = refundable_value - base_fee;
            payouts_promised.insert(
                to,
                ReservedPayout {
                    gas_limit,
                    tx_value,
                    total_refundable_value: refundable_value,
                },
            );
        }

        let coinbase_diff_before_payouts = {
            let coinbase_balance_after = self.state.balance(ctx.block_env.coinbase)?;
            coinbase_balance_after
                .checked_sub(coinbase_balance_before)
                .unwrap_or_default()
        };
        let total_payouts_promissed = payouts_promised
            .values()
            .map(|v| v.total_refundable_value)
            .sum::<U256>();

        Ok(Ok(ShareBundleCommitResult {
            bundle_ok: insert,
            coinbase_diff_before_payouts,
            total_payouts_promissed,
            payouts_promissed: payouts_promised,
        }))
    }

    fn get_used_state_trace(&mut self) -> Option<UsedStateTrace> {
        self.tracer
            .as_mut()
            .and_then(|t| t.get_used_state_tracer())
            .map(|t| t.clone())
    }

    pub fn commit_order(
        &mut self,
        order: &Order,
        ctx: &BlockBuildingContext,
        cumulative_gas_used: u64,
        gas_reserved: u64,
        cumulative_blob_gas_used: u64,
        allow_tx_skip: bool,
    ) -> Result<Result<OrderOk, OrderErr>, CriticalCommitOrderError> {
        self.execute_with_rollback(|s| {
            s.commit_order_no_rollback(
                order,
                ctx,
                cumulative_gas_used,
                gas_reserved,
                cumulative_blob_gas_used,
                allow_tx_skip,
            )
        })
    }

    fn commit_order_no_rollback(
        &mut self,
        order: &Order,
        ctx: &BlockBuildingContext,
        cumulative_gas_used: u64,
        gas_reserved: u64,
        cumulative_blob_gas_used: u64,
        allow_tx_skip: bool,
    ) -> Result<Result<OrderOk, OrderErr>, CriticalCommitOrderError> {
        let coinbase_balance_before = self.state.balance(ctx.block_env.coinbase)?;
        match order {
            Order::Tx(tx) => {
                let res = self.commit_tx(
                    &tx.tx_with_blobs,
                    ctx,
                    cumulative_gas_used,
                    gas_reserved,
                    cumulative_blob_gas_used,
                )?;
                match res {
                    Ok(ok) => {
                        // Builder does not sign txs in this code path, so allow negative coinbase
                        // profit.
                        let coinbase_balance_after = self.state.balance(ctx.block_env.coinbase)?;
                        let coinbase_profit =
                            coinbase_balance_after.saturating_sub(coinbase_balance_before);
                        Ok(Ok(OrderOk {
                            coinbase_profit,
                            gas_used: ok.gas_used,
                            cumulative_gas_used: ok.cumulative_gas_used,
                            blob_gas_used: ok.blob_gas_used,
                            cumulative_blob_gas_used: ok.cumulative_blob_gas_used,
                            txs: vec![ok.tx],
                            nonces_updated: vec![ok.nonce_updated],
                            receipts: vec![ok.receipt],
                            paid_kickbacks: Vec::new(),
                            used_state_trace: self.get_used_state_trace(),
                            original_order_ids: Vec::new(),
                        }))
                    }
                    Err(err) => Ok(Err(err.into())),
                }
            }
            Order::Bundle(bundle) => {
                let res = self.commit_bundle(
                    bundle,
                    ctx,
                    cumulative_gas_used,
                    gas_reserved,
                    cumulative_blob_gas_used,
                    allow_tx_skip,
                )?;
                match res {
                    Ok(ok) => {
                        // Builder does not sign txs in this code path, so allow negative coinbase
                        // profit.
                        let coinbase_balance_after = self.state.balance(ctx.block_env.coinbase)?;
                        let coinbase_profit =
                            coinbase_balance_after.saturating_sub(coinbase_balance_before);
                        Ok(Ok(OrderOk {
                            coinbase_profit,
                            gas_used: ok.gas_used,
                            cumulative_gas_used: ok.cumulative_gas_used,
                            blob_gas_used: ok.blob_gas_used,
                            cumulative_blob_gas_used: ok.cumulative_blob_gas_used,
                            txs: ok.txs,
                            nonces_updated: ok.nonces_updated,
                            receipts: ok.receipts,
                            paid_kickbacks: ok.paid_kickbacks,
                            used_state_trace: self.get_used_state_trace(),
                            original_order_ids: ok.original_order_ids,
                        }))
                    }
                    Err(err) => Ok(Err(err.into())),
                }
            }
            Order::ShareBundle(bundle) => {
                let res = self.commit_share_bundle(
                    bundle,
                    ctx,
                    cumulative_gas_used,
                    gas_reserved,
                    cumulative_blob_gas_used,
                    allow_tx_skip,
                )?;
                match res {
                    Ok(ok) => {
                        let coinbase_balance_after = self.state.balance(ctx.block_env.coinbase)?;
                        // Builder does sign txs in this code path, so do not allow negative coinbase
                        // profit.
                        let coinbase_profit = match coinbase_profit(
                            coinbase_balance_before,
                            coinbase_balance_after,
                        ) {
                            Ok(profit) => profit,
                            Err(err) => {
                                return Ok(Err(err));
                            }
                        };
                        Ok(Ok(OrderOk {
                            coinbase_profit,
                            gas_used: ok.gas_used,
                            cumulative_gas_used: ok.cumulative_gas_used,
                            blob_gas_used: ok.blob_gas_used,
                            cumulative_blob_gas_used: ok.cumulative_blob_gas_used,
                            txs: ok.txs,
                            nonces_updated: ok.nonces_updated,
                            receipts: ok.receipts,
                            paid_kickbacks: ok.paid_kickbacks,
                            used_state_trace: self.get_used_state_trace(),
                            original_order_ids: ok.original_order_ids,
                        }))
                    }
                    Err(err) => Ok(Err(err.into())),
                }
            }
        }
    }
}

impl<'a, 'b> PartialBlockFork<'a, 'b, ()> {
    pub fn new(state: &'a mut BlockState) -> Self {
        Self {
            rollbacks: 0,
            state,
            tracer: None,
        }
    }
}

fn coinbase_profit(
    coinbase_balance_before: U256,
    coinbase_balance_after: U256,
) -> Result<U256, OrderErr> {
    if coinbase_balance_after >= coinbase_balance_before {
        Ok(coinbase_balance_after - coinbase_balance_before)
    } else {
        Err(OrderErr::NegativeProfit(
            coinbase_balance_before - coinbase_balance_after,
        ))
    }
}

fn update_nonce_list(nonces_updated: &mut Vec<(Address, u64)>, new_update: (Address, u64)) {
    for (addr, nonce) in &mut *nonces_updated {
        if addr == &new_update.0 {
            *nonce = new_update.1;
            return;
        }
    }
    nonces_updated.push(new_update);
}

fn update_nonce_list_with_updates(
    nonces_updated: &mut Vec<(Address, u64)>,
    new_updates: Vec<(Address, u64)>,
) {
    for new_update in new_updates {
        update_nonce_list(nonces_updated, new_update);
    }
}
