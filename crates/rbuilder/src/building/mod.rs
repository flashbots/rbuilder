pub mod block_orders;
pub mod builders;
pub mod built_block_trace;
#[cfg(test)]
pub mod conflict;
pub mod evm_inspector;
pub mod fmt;
pub mod order_commit;
pub mod payout_tx;
pub mod sim;
pub mod testing;
pub mod tracers;
use alloy_consensus::{Header, EMPTY_OMMER_ROOT_HASH};
use alloy_primitives::{Address, Bytes, U256};
use builders::mock_block_building_helper::MockRootHasher;
use reth_primitives::{BlockBody, BlockExt};

use crate::{
    primitives::{Order, OrderId, SimValue, SimulatedOrder, TransactionSignedEcRecoveredWithBlobs},
    provider::RootHasher,
    roothash::RootHashError,
    utils::{a2r_withdrawal, timestamp_as_u64, Signer},
};
use ahash::HashSet;
use alloy_eips::{
    calc_excess_blob_gas,
    eip1559::{calculate_block_gas_limit, ETHEREUM_BLOCK_GAS_LIMIT},
    eip4844::BlobTransactionSidecar,
    eip4895::Withdrawals,
    eip6110::DEPOSIT_REQUEST_TYPE,
    eip7002::WITHDRAWAL_REQUEST_TYPE,
    eip7251::CONSOLIDATION_REQUEST_TYPE,
    eip7685::Requests,
    merge::BEACON_NONCE,
};
use alloy_rpc_types_beacon::events::PayloadAttributesEvent;
use jsonrpsee::core::Serialize;
use reth::{
    payload::PayloadId,
    primitives::{proofs, Block, Receipt, Receipts, SealedBlock},
    providers::ExecutionOutcome,
    revm::cached::CachedReads,
};
use reth_basic_payload_builder::commit_withdrawals;
use reth_chainspec::{ChainSpec, EthereumHardforks};
use reth_errors::ProviderError;
use reth_evm::{env::EvmEnv, system_calls::SystemCaller, ConfigureEvmEnv, NextBlockEnvAttributes};
use reth_evm_ethereum::{
    eip6110::parse_deposits_from_receipts, revm_spec_by_timestamp_and_block_number, EthEvmConfig,
};
use reth_node_api::{EngineApiMessageVersion, PayloadBuilderAttributes};
use reth_payload_builder::EthPayloadBuilderAttributes;
use revm::{
    db::states::bundle_state::BundleRetention,
    primitives::{BlobExcessGasAndPrice, BlockEnv, CfgEnvWithHandlerCfg, SpecId},
};
use revm_primitives::InvalidTransaction;
use serde::Deserialize;
use std::{
    hash::Hash,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use time::OffsetDateTime;

use self::tracers::SimulationTracer;
use crate::utils::default_cfg_env;
pub use block_orders::*;
pub use built_block_trace::*;
#[cfg(test)]
pub use conflict::*;
pub use order_commit::*;
pub use payout_tx::*;
pub use sim::simulate_order;

#[derive(Debug, Clone)]
pub struct BlockBuildingContext {
    pub block_env: BlockEnv,
    pub initialized_cfg: CfgEnvWithHandlerCfg,
    pub attributes: EthPayloadBuilderAttributes,
    pub chain_spec: Arc<ChainSpec>,
    /// Signer to sign builder payoffs (end of block and mev-share).
    /// Is Option to avoid any possible bug (losing money!) with payoffs.
    /// None: coinbase = attributes.suggested_fee_recipient. No payoffs allowed.
    /// Some(signer): coinbase = signer.
    pub builder_signer: Option<Signer>,
    pub blocklist: HashSet<Address>,
    pub extra_data: Vec<u8>,
    /// Excess blob gas calculated from the parent block header
    pub excess_blob_gas: Option<u64>,
    /// Version of the EVM that we are going to use
    pub spec_id: SpecId,
    pub root_hasher: Arc<dyn RootHasher>,
}

impl BlockBuildingContext {
    #[allow(clippy::too_many_arguments)]
    /// spec_id None: we use the proper SpecId for the block timestamp.
    /// We are forced to return Option since next_cfg_and_block_env returns Result although it never fails! (reth v1.1.1)
    pub fn from_attributes(
        attributes: PayloadAttributesEvent,
        parent: &Header,
        signer: Signer,
        chain_spec: Arc<ChainSpec>,
        blocklist: HashSet<Address>,
        prefer_gas_limit: Option<u64>,
        extra_data: Vec<u8>,
        spec_id: Option<SpecId>,
        root_hasher: Arc<dyn RootHasher>,
    ) -> Option<BlockBuildingContext> {
        let attributes = EthPayloadBuilderAttributes::try_new(
            attributes.data.parent_block_hash,
            attributes.data.payload_attributes.clone(),
            EngineApiMessageVersion::default() as u8,
        )
        .expect("PayloadBuilderAttributes::try_new");
        let eth_evm_config = EthEvmConfig::new(chain_spec.clone());
        let gas_limit = calculate_block_gas_limit(
            parent.gas_limit,
            // This is only for tests, prefer_gas_limit should always be Some since
            // the protocol does NOT cap the block to ETHEREUM_BLOCK_GAS_LIMIT.
            prefer_gas_limit.unwrap_or(ETHEREUM_BLOCK_GAS_LIMIT),
        );
        let EvmEnv {
            cfg_env_with_handler_cfg,
            mut block_env,
        } = eth_evm_config
            .next_cfg_and_block_env(
                parent,
                NextBlockEnvAttributes {
                    timestamp: attributes.timestamp(),
                    suggested_fee_recipient: attributes.suggested_fee_recipient(),
                    prev_randao: attributes.prev_randao(),
                    gas_limit,
                },
            )
            .ok()?;
        block_env.coinbase = signer.address;

        let excess_blob_gas = if chain_spec.is_cancun_active_at_timestamp(attributes.timestamp) {
            if chain_spec.is_cancun_active_at_timestamp(parent.timestamp) {
                let parent_excess_blob_gas = parent.excess_blob_gas.unwrap_or_default();
                let parent_blob_gas_used = parent.blob_gas_used.unwrap_or_default();
                Some(calc_excess_blob_gas(
                    parent_excess_blob_gas,
                    parent_blob_gas_used,
                ))
            } else {
                // for the first post-fork block, both parent.blob_gas_used and
                // parent.excess_blob_gas are evaluated as 0
                Some(calc_excess_blob_gas(0, 0))
            }
        } else {
            None
        };
        let spec_id = spec_id.unwrap_or_else(|| {
            revm_spec_by_timestamp_and_block_number(
                &chain_spec,
                attributes.timestamp(),
                parent.number + 1,
            )
        });
        Some(BlockBuildingContext {
            block_env,
            initialized_cfg: cfg_env_with_handler_cfg,
            attributes,
            chain_spec,
            builder_signer: Some(signer),
            blocklist,
            extra_data,
            excess_blob_gas,
            spec_id,
            root_hasher,
        })
    }

    #[allow(clippy::too_many_arguments)]
    /// `from_block_data` is used to create `BlockBuildingContext` from onchain block for backtest purposes
    /// spec_id None: we use the SpecId for the block.
    /// Note: We calculate SpecId based on the current block instead of the parent block so this will break for the blocks +-1 relative to the fork
    pub fn from_onchain_block(
        onchain_block: alloy_rpc_types::Block,
        chain_spec: Arc<ChainSpec>,
        spec_id: Option<SpecId>,
        blocklist: HashSet<Address>,
        coinbase: Address,
        suggested_fee_recipient: Address,
        builder_signer: Option<Signer>,
        root_hasher: Arc<dyn RootHasher>,
    ) -> BlockBuildingContext {
        let block_number = onchain_block.header.number;

        let blob_excess_gas_and_price =
            if chain_spec.is_cancun_active_at_timestamp(onchain_block.header.timestamp) {
                Some(BlobExcessGasAndPrice::new(
                    onchain_block.header.excess_blob_gas.unwrap_or_default(),
                    chain_spec.is_prague_active_at_timestamp(onchain_block.header.timestamp),
                ))
            } else {
                None
            };
        let block_env = BlockEnv {
            number: U256::from(block_number),
            coinbase,
            timestamp: U256::from(onchain_block.header.timestamp),
            difficulty: onchain_block.header.difficulty,
            prevrandao: Some(onchain_block.header.mix_hash),
            basefee: U256::from(
                onchain_block
                    .header
                    .base_fee_per_gas
                    .expect("Failed to get basefee"),
            ), // TODO: improve
            gas_limit: U256::from(onchain_block.header.gas_limit),
            blob_excess_gas_and_price,
        };

        let cfg = default_cfg_env(&chain_spec, timestamp_as_u64(&onchain_block), block_number);

        let withdrawals = Withdrawals::new(
            onchain_block
                .withdrawals
                .clone()
                .map(|w| w.into_iter().map(a2r_withdrawal).collect::<Vec<_>>())
                .unwrap_or_default(),
        );

        let attributes = EthPayloadBuilderAttributes {
            id: PayloadId::new([0u8; 8]),
            parent: onchain_block.header.parent_hash,
            timestamp: timestamp_as_u64(&onchain_block),
            suggested_fee_recipient,
            prev_randao: onchain_block.header.mix_hash,
            withdrawals,
            parent_beacon_block_root: onchain_block.header.parent_beacon_block_root,
        };
        let spec_id = spec_id.unwrap_or_else(|| {
            // we use current block data instead of the parent block data to determine fork
            // this will break for one block after the fork
            revm_spec_by_timestamp_and_block_number(
                &chain_spec,
                onchain_block.header.timestamp,
                onchain_block.header.number,
            )
        });
        BlockBuildingContext {
            block_env,
            initialized_cfg: cfg,
            attributes,
            chain_spec,
            builder_signer,
            blocklist,
            extra_data: Vec::new(),
            excess_blob_gas: onchain_block.header.excess_blob_gas,
            spec_id,
            root_hasher,
        }
    }

    /// Useless BlockBuildingContext for testing in contexts where we can't avoid having a BlockBuildingContext.
    pub fn dummy_for_testing() -> Self {
        let mut onchain_block: alloy_rpc_types::Block = Default::default();
        onchain_block.header.base_fee_per_gas = Some(0);
        BlockBuildingContext::from_onchain_block(
            onchain_block,
            reth_chainspec::MAINNET.clone(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Arc::new(MockRootHasher {}),
        )
    }

    pub fn modify_use_suggested_fee_recipient_as_coinbase(&mut self) {
        self.builder_signer = None;
        self.block_env.coinbase = self.attributes.suggested_fee_recipient;
    }

    pub fn timestamp(&self) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(self.attributes.timestamp as i64)
            .expect("Payload attributes timestamp")
    }

    pub fn block(&self) -> u64 {
        self.block_env.number.to()
    }

    pub fn coinbase_is_suggested_fee_recipient(&self) -> bool {
        self.block_env.coinbase == self.attributes.suggested_fee_recipient
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BlockBuildingConfig {
    pub sorting: Sorting,
    pub discard_txs: bool,
    // failed orders are not tried for the subsequent iterations
    pub remove_failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sorting {
    /// Sorts the SimulatedOrders by its effective gas price. This not only includes the explicit gas price set in the tx but also the direct coinbase payments
    /// so we compute it as (coinbase balance delta after executing the order) / (gas used)
    MevGasPrice,
    /// Sorts the SimulatedOrders by its absolute profit which is computed as the coinbase balance delta after executing the order
    MaxProfit,
}

impl Sorting {
    pub fn sorting_value(&self, sim_value: &SimValue) -> U256 {
        match self {
            Sorting::MevGasPrice => sim_value.mev_gas_price,
            Sorting::MaxProfit => sim_value.coinbase_profit,
        }
    }
}

impl FromStr for Sorting {
    type Err = eyre::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "mev_gas_price" => Ok(Self::MevGasPrice),
            "max_profit" => Ok(Self::MaxProfit),
            _ => eyre::bail!("Invalid algorithm"),
        }
    }
}
impl std::fmt::Display for Sorting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Sorting::MevGasPrice => write!(f, "mev_gas_price"),
            Sorting::MaxProfit => write!(f, "max_profit"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PartialBlock<Tracer: SimulationTracer> {
    /// Value used as allow_tx_skip on calls to [`PartialBlockFork`]
    pub discard_txs: bool,
    /// If some [`enforce_inplace_sim_result`] is called after each tx to check the profit.
    pub enforce_sorting: Option<Sorting>,
    pub gas_used: u64,
    /// Reserved gas for later use (usually final payout tx). When simulating we subtract this from the block gas limit.
    pub gas_reserved: u64,
    pub blob_gas_used: u64,
    /// Updated after each order.
    pub coinbase_profit: U256,
    /// Txs belonging to successfully executed orders.
    pub executed_tx: Vec<TransactionSignedEcRecoveredWithBlobs>,
    /// Receipts belonging to successfully executed orders.
    pub receipts: Vec<Receipt>,
    pub tracer: Tracer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionResult {
    pub coinbase_profit: U256,
    pub inplace_sim: SimValue,
    pub gas_used: u64,
    pub order: Order,
    pub txs: Vec<TransactionSignedEcRecoveredWithBlobs>,
    /// Patch to get the executed OrderIds for merged sbundles (see: [`BundleOk::original_order_ids`],[`ShareBundleMerger`] )
    /// Fully dropped orders (TxRevertBehavior::AllowedExcluded allows it!) are not included.
    pub original_order_ids: Vec<OrderId>,
    pub receipts: Vec<Receipt>,
    pub nonces_updated: Vec<(Address, u64)>,
    pub paid_kickbacks: Vec<(Address, U256)>,
}

#[derive(Error, Debug)]
pub enum InsertPayoutTxErr {
    #[error("Critical order commit error: {0}")]
    CriticalCommitError(#[from] CriticalCommitOrderError),
    #[error("Profit too low to insert payout tx")]
    ProfitTooLow,
    #[error("Payout tx reverted")]
    PayoutTxReverted,
    #[error("Signer error: {0}")]
    SignerError(#[from] secp256k1::Error),
    #[error("Tx error: {0}")]
    TxErr(#[from] TransactionErr),
    #[error("Payout without signer")]
    NoSigner,
}

#[derive(Error, Debug)]
pub enum ExecutionError {
    #[error("Order error: {0}")]
    OrderError(#[from] OrderErr),
    #[error("Lower inserted value, before: {before:?}, inplace: {inplace:?}")]
    LowerInsertedValue { before: SimValue, inplace: SimValue },
}

impl ExecutionError {
    /// If error is NonceTooHigh returns nonce of the transaction
    pub fn try_get_tx_too_high_error(&self, order: &Order) -> Option<(Address, u64)> {
        match self {
            ExecutionError::OrderError(OrderErr::Transaction(
                TransactionErr::InvalidTransaction(InvalidTransaction::NonceTooHigh {
                    tx: tx_nonce,
                    ..
                }),
            )) => Some((order.list_txs().first()?.0.signer(), *tx_nonce)),
            ExecutionError::OrderError(OrderErr::Bundle(BundleErr::InvalidTransaction(
                hash,
                TransactionErr::InvalidTransaction(InvalidTransaction::NonceTooHigh {
                    tx: tx_nonce,
                    ..
                }),
            ))) => {
                let signer = order
                    .list_txs()
                    .iter()
                    .find(|(tx, _)| TransactionSignedEcRecoveredWithBlobs::hash(tx) == *hash)?
                    .0
                    .signer();
                Some((signer, *tx_nonce))
            }
            _ => None,
        }
    }
}

pub struct FinalizeResult {
    pub sealed_block: SealedBlock,
    pub cached_reads: CachedReads,
    // sidecars for all txs in SealedBlock
    pub txs_blob_sidecars: Vec<Arc<BlobTransactionSidecar>>,
    /// The Pectra execution requests for this bid.
    pub execution_requests: Vec<Bytes>,

    pub root_hash_time: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum FinalizeError {
    #[error("Root hash error: {0:?}")]
    RootHash(#[from] RootHashError),
    #[error("Other error: {0:?}")]
    Other(#[from] eyre::Report),
}

impl FinalizeError {
    /// see `RootHashError::is_consistent_db_view_err`
    pub fn is_consistent_db_view_err(&self) -> bool {
        match self {
            FinalizeError::RootHash(root_hash) => root_hash.is_consistent_db_view_err(),
            FinalizeError::Other(_) => false,
        }
    }
}

impl<Tracer: SimulationTracer> PartialBlock<Tracer> {
    pub fn with_tracer<NewTracer: SimulationTracer>(
        self,
        tracer: NewTracer,
    ) -> PartialBlock<NewTracer> {
        PartialBlock {
            discard_txs: self.discard_txs,
            enforce_sorting: self.enforce_sorting,
            gas_used: self.gas_used,
            gas_reserved: self.gas_reserved,
            blob_gas_used: self.blob_gas_used,
            coinbase_profit: self.coinbase_profit,
            executed_tx: self.executed_tx,
            receipts: self.receipts,
            tracer,
        }
    }

    pub fn reserve_gas(&mut self, gas: u64) {
        self.gas_reserved = gas;
    }

    pub fn free_reserved_gas(&mut self) {
        self.gas_reserved = 0;
    }

    pub fn commit_order(
        &mut self,
        order: &SimulatedOrder,
        ctx: &BlockBuildingContext,
        state: &mut BlockState,
    ) -> Result<Result<ExecutionResult, ExecutionError>, CriticalCommitOrderError> {
        if ctx.builder_signer.is_none() && !order.sim_value.paid_kickbacks.is_empty() {
            // Return here to avoid wasting time on a call to fork.commit_order that 99% will fail
            return Ok(Err(ExecutionError::OrderError(OrderErr::Bundle(
                BundleErr::NoSigner,
            ))));
        }

        let mut fork = PartialBlockFork::new(state).with_tracer(&mut self.tracer);
        let rollback = fork.rollback_point();
        let exec_result = fork.commit_order(
            &order.order,
            ctx,
            self.gas_used,
            self.gas_reserved,
            self.blob_gas_used,
            self.discard_txs,
        )?;
        let ok_result = match exec_result {
            Ok(ok) => ok,
            Err(err) => {
                return Ok(Err(err.into()));
            }
        };

        let inplace_sim_result = SimValue::new(
            ok_result.coinbase_profit,
            ok_result.gas_used,
            ok_result.blob_gas_used,
            ok_result.paid_kickbacks.clone(),
        );
        if let Some(enforce_sorting) = self.enforce_sorting {
            match enforce_inplace_sim_result(enforce_sorting, &order.sim_value, &inplace_sim_result)
            {
                Ok(()) => {}
                Err(err) => {
                    fork.rollback(rollback);
                    return Ok(Err(err));
                }
            }
        }

        self.gas_used += ok_result.gas_used;
        self.blob_gas_used += ok_result.blob_gas_used;
        self.coinbase_profit += ok_result.coinbase_profit;
        self.executed_tx.extend(ok_result.txs.clone());
        self.receipts.extend(ok_result.receipts.clone());
        Ok(Ok(ExecutionResult {
            coinbase_profit: ok_result.coinbase_profit,
            inplace_sim: inplace_sim_result,
            gas_used: ok_result.gas_used,
            order: order.order.clone(),
            txs: ok_result.txs,
            original_order_ids: ok_result.original_order_ids,
            receipts: ok_result.receipts,
            nonces_updated: ok_result.nonces_updated,
            paid_kickbacks: ok_result.paid_kickbacks,
        }))
    }

    /// Gets the block profit excluding the expected payout base gas that we'll pay.
    pub fn get_proposer_payout_tx_value(
        &self,
        gas_limit: u64,
        ctx: &BlockBuildingContext,
    ) -> Result<U256, InsertPayoutTxErr> {
        self.coinbase_profit
            .checked_sub(U256::from(gas_limit) * ctx.block_env.basefee)
            .ok_or(InsertPayoutTxErr::ProfitTooLow)
    }

    /// Inserts payout tx to ctx.attributes.suggested_fee_recipient (should be called at the end of the block)
    /// Returns the paid value (block profit after subtracting the burned basefee of the payout tx)
    pub fn insert_proposer_payout_tx(
        &mut self,
        gas_limit: u64,
        value: U256,
        ctx: &BlockBuildingContext,
        state: &mut BlockState,
    ) -> Result<(), InsertPayoutTxErr> {
        let builder_signer = ctx
            .builder_signer
            .as_ref()
            .ok_or(InsertPayoutTxErr::NoSigner)?;
        self.free_reserved_gas();
        let nonce = state
            .nonce(builder_signer.address)
            .map_err(CriticalCommitOrderError::Reth)?;
        let tx = create_payout_tx(
            ctx.chain_spec.as_ref(),
            ctx.block_env.basefee,
            builder_signer,
            nonce,
            ctx.attributes.suggested_fee_recipient,
            gas_limit,
            value.to(),
        )?;
        // payout tx has no blobs so it's safe to unwrap
        let tx = TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx).unwrap();
        let mut fork = PartialBlockFork::new(state).with_tracer(&mut self.tracer);
        let exec_result = fork.commit_tx(&tx, ctx, self.gas_used, 0, self.blob_gas_used)?;
        let ok_result = exec_result?;
        if !ok_result.receipt.success {
            return Err(InsertPayoutTxErr::PayoutTxReverted);
        }

        self.gas_used += ok_result.gas_used;
        self.blob_gas_used += ok_result.blob_gas_used;
        self.executed_tx.push(ok_result.tx);
        self.receipts.push(ok_result.receipt);

        Ok(())
    }

    /// Mostly based on reth's (v1.1.1) default_ethereum_payload_builder.
    #[allow(clippy::too_many_arguments)]
    pub fn finalize(
        self,
        state: &mut BlockState,
        ctx: &BlockBuildingContext,
    ) -> Result<FinalizeResult, FinalizeError> {
        let requests = if ctx
            .chain_spec
            .is_prague_active_at_timestamp(ctx.attributes.timestamp())
        {
            let mut system_caller = SystemCaller::new(
                EthEvmConfig::new(ctx.chain_spec.clone()),
                ctx.chain_spec.clone(),
            );
            let mut db = state.new_db_ref();

            let deposit_requests =
                parse_deposits_from_receipts(&ctx.chain_spec, self.receipts.iter())
                    .map_err(|err| FinalizeError::Other(err.into()))?;
            let withdrawal_requests = system_caller
                .post_block_withdrawal_requests_contract_call(
                    db.as_mut(),
                    &ctx.initialized_cfg,
                    &ctx.block_env,
                )
                .map_err(|err| FinalizeError::Other(err.into()))?;
            let consolidation_requests = system_caller
                .post_block_consolidation_requests_contract_call(
                    db.as_mut(),
                    &ctx.initialized_cfg,
                    &ctx.block_env,
                )
                .map_err(|err| FinalizeError::Other(err.into()))?;

            let mut requests = Requests::default();
            if !deposit_requests.is_empty() {
                requests.push_request_with_type(DEPOSIT_REQUEST_TYPE, deposit_requests);
            }
            if !withdrawal_requests.is_empty() {
                requests.push_request_with_type(WITHDRAWAL_REQUEST_TYPE, withdrawal_requests);
            }
            if !consolidation_requests.is_empty() {
                requests.push_request_with_type(CONSOLIDATION_REQUEST_TYPE, consolidation_requests);
            }
            Some(requests)
        } else {
            None
        };

        let withdrawals_root = {
            let mut db = state.new_db_ref();
            let withdrawals_root = commit_withdrawals(
                db.as_mut(),
                &ctx.chain_spec,
                ctx.attributes.timestamp,
                &ctx.attributes.withdrawals,
            )
            .map_err(|err| FinalizeError::Other(err.into()))?;
            // merge all transitions into bundle state, this would apply the withdrawal balance changes
            // and 4788 contract call
            db.as_mut().merge_transitions(BundleRetention::Reverts);
            withdrawals_root
        };

        let (cached_reads, bundle) = state.clone_bundle_and_cache();
        let block_number = ctx.block_env.number.to::<u64>();

        let requests_hash = requests.as_ref().map(|requests| requests.requests_hash());
        let execution_outcome = ExecutionOutcome::new(
            bundle,
            Receipts::from(vec![self
                .receipts
                .into_iter()
                .map(Option::Some)
                .collect::<Vec<_>>()]),
            block_number,
            vec![requests.clone().unwrap_or_default()],
        );

        // @TODO: Check ethereum_receipts_root since it could fail on Op. Check reth crates/optimism/payload/src/builder.rs?
        let receipts_root = execution_outcome
            .ethereum_receipts_root(block_number)
            .expect("Number is in range");
        let logs_bloom = execution_outcome
            .block_logs_bloom(block_number)
            .expect("Number is in range");

        // calculate the state root
        let start = Instant::now();
        let state_root = ctx.root_hasher.state_root(&execution_outcome)?;
        let root_hash_time = start.elapsed();

        // create the block header
        let transactions_root = proofs::calculate_transaction_root(&self.executed_tx);

        // double check blocked txs
        for tx_with_blob in &self.executed_tx {
            if ctx.blocklist.contains(&tx_with_blob.signer()) {
                return Err(FinalizeError::Other(eyre::eyre!(
                    "To from blocked address."
                )));
            }
            if let Some(to) = tx_with_blob.to() {
                if ctx.blocklist.contains(&to) {
                    return Err(FinalizeError::Other(eyre::eyre!("Tx to blocked address")));
                }
            }
        }

        let mut txs_blob_sidecars = Vec::new();
        let (excess_blob_gas, blob_gas_used) = if ctx
            .chain_spec
            .is_cancun_active_at_timestamp(ctx.attributes.timestamp)
        {
            for tx_with_blob in &self.executed_tx {
                if !tx_with_blob.blobs_sidecar.blobs.is_empty() {
                    txs_blob_sidecars.push(tx_with_blob.blobs_sidecar.clone());
                }
            }
            (ctx.excess_blob_gas, Some(self.blob_gas_used))
        } else {
            (None, None)
        };

        let header = Header {
            parent_hash: ctx.attributes.parent,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: ctx.block_env.coinbase,
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root,
            logs_bloom,
            timestamp: ctx.attributes.timestamp,
            mix_hash: ctx.attributes.prev_randao,
            nonce: BEACON_NONCE.into(),
            base_fee_per_gas: Some(ctx.block_env.basefee.to()),
            number: block_number,
            gas_limit: ctx.block_env.gas_limit.to(),
            difficulty: U256::ZERO,
            gas_used: self.gas_used,
            extra_data: ctx.extra_data.clone().into(),
            parent_beacon_block_root: ctx.attributes.parent_beacon_block_root,
            blob_gas_used,
            excess_blob_gas,
            requests_hash,
        };

        let withdrawals = ctx
            .chain_spec
            .is_shanghai_active_at_timestamp(ctx.attributes.timestamp)
            .then(|| ctx.attributes.withdrawals.clone());

        // seal the block
        let block = Block {
            header,
            body: BlockBody {
                transactions: self
                    .executed_tx
                    .into_iter()
                    .map(|t| t.into_internal_tx_unsecure().into())
                    .collect(),
                ommers: vec![],
                withdrawals,
            },
        };

        Ok(FinalizeResult {
            sealed_block: block.seal_slow(),
            cached_reads,
            txs_blob_sidecars,
            root_hash_time,
            execution_requests: requests.map(|er| er.take()).unwrap_or_default(),
        })
    }

    pub fn pre_block_call(
        &mut self,
        ctx: &BlockBuildingContext,
        state: &mut BlockState,
    ) -> eyre::Result<()> {
        let mut db = state.new_db_ref();
        let mut system_caller = SystemCaller::new(
            EthEvmConfig::new(ctx.chain_spec.clone()),
            ctx.chain_spec.clone(),
        );
        system_caller.pre_block_beacon_root_contract_call(
            db.as_mut(),
            &ctx.initialized_cfg,
            &ctx.block_env,
            ctx.attributes.parent_beacon_block_root(),
        )?;
        system_caller.pre_block_blockhashes_contract_call(
            db.as_mut(),
            &ctx.initialized_cfg,
            &ctx.block_env,
            ctx.attributes.parent,
        )?;
        db.as_mut().merge_transitions(BundleRetention::Reverts);
        Ok(())
    }
}

impl PartialBlock<()> {
    pub fn new(discard_txs: bool, enforce_sorting: Option<Sorting>) -> Self {
        Self {
            discard_txs,
            enforce_sorting,
            gas_used: 0,
            gas_reserved: 0,
            blob_gas_used: 0,
            coinbase_profit: U256::ZERO,
            executed_tx: Vec::new(),
            receipts: Vec::new(),
            tracer: (),
        }
    }
}

#[derive(Error, Debug)]
pub enum FillOrdersError {
    #[error("Reth error: {0}")]
    RethError(#[from] ProviderError),
    #[error("Estimate payout gas error: {0}")]
    EstimatePayoutGasErr(#[from] EstimatePayoutGasErr),
    #[error("Critical commit order error: {0}")]
    CriticalCommitOrderError(#[from] CriticalCommitOrderError),
    #[error("Payout tx error: {0}")]
    PayoutTxErr(#[from] InsertPayoutTxErr),
}

// Enforces that 'inplace' simulation results during block building are not lower than 95% of the top-of-block simulation results
// @Opt is large err OK here
#[allow(clippy::result_large_err)]
fn enforce_inplace_sim_result(
    sort: Sorting,
    sim_result: &SimValue,
    inplace_sim_result: &SimValue,
) -> Result<(), ExecutionError> {
    let (sim_value, inplace_value) = (
        sort.sorting_value(sim_result),
        sort.sorting_value(inplace_sim_result),
    );
    if (inplace_value * U256::from(100)) < (sim_value * U256::from(95)) {
        Err(ExecutionError::LowerInsertedValue {
            before: sim_result.clone(),
            inplace: inplace_sim_result.clone(),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enforce_inplace_sim_result_max_profit() {
        let sort = Sorting::MaxProfit;
        let sim_result = &SimValue {
            coinbase_profit: U256::from(100),
            mev_gas_price: U256::from(0),
            ..Default::default()
        };
        let inplace_sim_result = &SimValue {
            coinbase_profit: U256::from(94),
            mev_gas_price: U256::from(0),
            ..Default::default()
        };

        // Lower than 95% of the original value
        assert!(enforce_inplace_sim_result(sort, sim_result, inplace_sim_result).is_err());

        // Equal to original value
        let inplace_sim_result = &SimValue {
            coinbase_profit: U256::from(100),
            mev_gas_price: U256::from(0),
            ..Default::default()
        };
        assert!(enforce_inplace_sim_result(sort, sim_result, inplace_sim_result).is_ok());

        // Higher than original value
        let inplace_sim_result = &SimValue {
            coinbase_profit: U256::from(105),
            mev_gas_price: U256::from(0),
            ..Default::default()
        };
        assert!(enforce_inplace_sim_result(sort, sim_result, inplace_sim_result).is_ok());
    }

    #[test]
    fn test_enforce_inplace_sim_result_mev_gas_price() {
        let sort = Sorting::MevGasPrice;
        let sim_result = &SimValue {
            coinbase_profit: U256::from(0),
            mev_gas_price: U256::from(100),
            gas_used: 100,
            ..Default::default()
        };

        // Lower than 95% of the original value
        let inplace_sim_result = &SimValue {
            coinbase_profit: U256::from(0),
            mev_gas_price: U256::from(94),
            gas_used: 94,
            ..Default::default()
        };
        assert!(enforce_inplace_sim_result(sort, sim_result, inplace_sim_result).is_err());

        // Equal to original value
        let inplace_sim_result = &SimValue {
            coinbase_profit: U256::from(0),
            mev_gas_price: U256::from(100),
            gas_used: 105,
            ..Default::default()
        };
        assert!(enforce_inplace_sim_result(sort, sim_result, inplace_sim_result).is_ok());

        // Higher than original value
        let inplace_sim_result = &SimValue {
            coinbase_profit: U256::from(0),
            mev_gas_price: U256::from(105),
            gas_used: 105,
            ..Default::default()
        };
        assert!(enforce_inplace_sim_result(sort, sim_result, inplace_sim_result).is_ok());
    }
}
