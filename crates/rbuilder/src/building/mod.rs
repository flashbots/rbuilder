use crate::{
    live_builder::{
        block_list_provider::BlockList, order_input::mempool_txs_detector::MempoolTxsDetector,
        payload_events::InternalPayloadId,
    },
    primitives::{Order, OrderId, SimValue, SimulatedOrder, TransactionSignedEcRecoveredWithBlobs},
    provider::RootHasher,
    roothash::RootHashError,
    utils::{
        a2r_withdrawal,
        constants::BASE_TX_GAS,
        default_cfg_env, elapsed_ms,
        receipts::{
            calculate_receipt_root_and_block_logs_bloom, calculate_transactions_root, BloomCache,
            TransactionRootCache,
        },
        timestamp_as_u64, Signer,
    },
};
use alloy_consensus::{constants::KECCAK_EMPTY, Header, EMPTY_OMMER_ROOT_HASH};
use alloy_eips::{
    eip1559::{calculate_block_gas_limit, ETHEREUM_BLOCK_GAS_LIMIT_30M},
    eip4844::BlobTransactionSidecar,
    eip4895::Withdrawals,
    eip7685::Requests,
    eip7840::BlobParams,
    merge::BEACON_NONCE,
};
use alloy_evm::{block::system_calls::SystemCaller, env::EvmEnv, eth::eip6110};
use alloy_primitives::{Address, Bytes, B256, I256, U256};
use alloy_rpc_types_beacon::events::PayloadAttributesEvent;
use cached_reads::{LocalCachedReads, SharedCachedReads};
use eth_sparse_mpt::SparseTrieLocalCache;
use evm::EthCachedEvmFactory;
use jsonrpsee::core::Serialize;
use reth::{
    payload::PayloadId,
    primitives::{Block, SealedBlock},
    providers::ExecutionOutcome,
};
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_errors::{BlockExecutionError, BlockValidationError, ProviderError};
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_evm_ethereum::{revm_spec_by_timestamp_and_block_number, EthEvmConfig};
use reth_node_api::{EngineApiMessageVersion, PayloadBuilderAttributes};
use reth_payload_builder::EthPayloadBuilderAttributes;
use reth_primitives::BlockBody;
use reth_primitives_traits::{proofs, Block as _};
use revm::{
    context::BlockEnv,
    context_interface::{block::BlobExcessGasAndPrice, result::InvalidTransaction},
    database::states::bundle_state::BundleRetention,
    primitives::hardfork::SpecId,
};
use serde::Deserialize;
use std::{
    collections::{hash_map, HashMap},
    hash::Hash,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use time::OffsetDateTime;
use tracing::{error, trace};
use tx_sim_cache::TxExecutionCache;

pub mod block_orders;
pub mod builders;
pub mod built_block_trace;
pub mod cached_reads;
#[cfg(test)]
pub mod conflict;
pub mod evm;
pub mod evm_inspector;
pub mod fmt;
pub mod order_commit;
pub mod payout_tx;
pub mod precompile_cache;
pub mod sim;
pub mod testing;
pub mod tracers;
pub mod tx_sim_cache;

pub use self::{
    block_orders::*, builders::mock_block_building_helper::MockRootHasher, built_block_trace::*,
    order_commit::*, payout_tx::*, sim::simulate_order, tracers::SimulationTracer,
};

#[cfg(test)]
pub use conflict::*;

#[derive(Debug, Clone)]
pub struct BlockBuildingContext {
    pub evm_factory: EthCachedEvmFactory,
    pub evm_env: EvmEnv,
    pub attributes: EthPayloadBuilderAttributes,
    pub chain_spec: Arc<ChainSpec>,
    /// cached chain_spec.blob_params_at_timestamp(attributes.timestamp()).max_blob_gas_per_block()
    max_blob_gas_per_block: u64,
    /// Signer to sign builder payoffs (end of block and mev-share).
    /// Is Option to avoid any possible bug (losing money!) with payoffs.
    /// None: coinbase = attributes.suggested_fee_recipient. No payoffs allowed.
    /// Some(signer): coinbase = signer.
    pub builder_signer: Option<Signer>,
    pub blocklist: BlockList,
    pub extra_data: Vec<u8>,
    /// Excess blob gas calculated from the parent block header
    pub excess_blob_gas: Option<u64>,
    /// Version of the EVM that we are going to use
    pub spec_id: SpecId,
    pub root_hasher: Arc<dyn RootHasher>,
    pub payload_id: InternalPayloadId,
    pub shared_cached_reads: Arc<SharedCachedReads>,
    pub tx_execution_cache: Arc<TxExecutionCache>,
    pub mempool_tx_detector: Arc<MempoolTxsDetector>,
    pub faster_finalize: bool,
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
        blocklist: BlockList,
        prefer_gas_limit: Option<u64>,
        extra_data: Vec<u8>,
        spec_id: Option<SpecId>,
        root_hasher: Arc<dyn RootHasher>,
        payload_id: InternalPayloadId,
        evm_caching_enable: bool,
        faster_finalize: bool,
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
            prefer_gas_limit.unwrap_or(ETHEREUM_BLOCK_GAS_LIMIT_30M),
        );
        let mut evm_env = eth_evm_config
            .next_evm_env(
                parent,
                &NextBlockEnvAttributes {
                    timestamp: attributes.timestamp(),
                    suggested_fee_recipient: attributes.suggested_fee_recipient(),
                    prev_randao: attributes.prev_randao(),
                    gas_limit,
                    withdrawals: Some(attributes.withdrawals.clone()),
                    parent_beacon_block_root: attributes.parent_beacon_block_root,
                },
            )
            .ok()?;
        evm_env.block_env.beneficiary = signer.address;

        let excess_blob_gas = if chain_spec.is_cancun_active_at_timestamp(attributes.timestamp) {
            if chain_spec.is_cancun_active_at_timestamp(parent.timestamp) {
                let blob_params = if chain_spec.is_prague_active_at_timestamp(attributes.timestamp)
                {
                    BlobParams::prague()
                } else {
                    BlobParams::cancun()
                };
                parent.next_block_excess_blob_gas(blob_params)
            } else {
                // for the first post-fork block, both parent.blob_gas_used and
                // parent.excess_blob_gas are evaluated as 0
                Some(alloy_eips::eip4844::calc_excess_blob_gas(0, 0))
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
        let max_blob_gas_per_block =
            Self::max_blob_gas_per_block_at(&chain_spec, attributes.timestamp());
        Some(BlockBuildingContext {
            evm_factory: EthCachedEvmFactory::default(),
            evm_env,
            attributes,
            chain_spec,
            builder_signer: Some(signer),
            blocklist,
            extra_data,
            excess_blob_gas,
            spec_id,
            root_hasher,
            payload_id,
            shared_cached_reads: Default::default(),
            tx_execution_cache: Arc::new(TxExecutionCache::new(evm_caching_enable)),
            max_blob_gas_per_block,
            mempool_tx_detector: Arc::new(MempoolTxsDetector::new()),
            faster_finalize,
        })
    }

    fn max_blob_gas_per_block_at(chain_spec: &ChainSpec, timestamp: u64) -> u64 {
        chain_spec
            .blob_params_at_timestamp(timestamp)
            .map(|params| params.max_blob_gas_per_block())
            .unwrap_or(0)
    }

    #[allow(clippy::too_many_arguments)]
    /// `from_block_data` is used to create `BlockBuildingContext` from onchain block for backtest purposes
    /// spec_id None: we use the SpecId for the block.
    /// Note: We calculate SpecId based on the current block instead of the parent block so this will break for the blocks +-1 relative to the fork
    pub fn from_onchain_block(
        onchain_block: alloy_rpc_types::Block,
        chain_spec: Arc<ChainSpec>,
        spec_id: Option<SpecId>,
        blocklist: BlockList,
        beneficiary: Address,
        suggested_fee_recipient: Address,
        builder_signer: Option<Signer>,
        root_hasher: Arc<dyn RootHasher>,
        evm_caching_enable: bool,
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
            number: block_number,
            beneficiary,
            timestamp: onchain_block.header.timestamp,
            difficulty: onchain_block.header.difficulty,
            prevrandao: Some(onchain_block.header.mix_hash),
            basefee: onchain_block
                .header
                .base_fee_per_gas
                .expect("Failed to get basefee"), // TODO: improve
            gas_limit: onchain_block.header.gas_limit,
            blob_excess_gas_and_price,
        };
        let cfg = default_cfg_env(&chain_spec, timestamp_as_u64(&onchain_block), block_number);
        // @TODO: revise
        let evm_env = EvmEnv::from((cfg, block_env));

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
        let max_blob_gas_per_block =
            Self::max_blob_gas_per_block_at(&chain_spec, attributes.timestamp());
        BlockBuildingContext {
            evm_factory: EthCachedEvmFactory::default(),
            evm_env,
            attributes,
            chain_spec,
            builder_signer,
            blocklist,
            extra_data: Vec::new(),
            excess_blob_gas: onchain_block.header.excess_blob_gas,
            spec_id,
            root_hasher,
            payload_id: 0,
            shared_cached_reads: Default::default(),
            tx_execution_cache: Arc::new(TxExecutionCache::new(evm_caching_enable)),
            max_blob_gas_per_block,
            mempool_tx_detector: Arc::new(MempoolTxsDetector::new()),
            faster_finalize: true,
        }
    }

    pub fn max_blob_gas_per_block(&self) -> u64 {
        self.max_blob_gas_per_block
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
            false,
        )
    }

    pub fn modify_use_suggested_fee_recipient_as_coinbase(&mut self) {
        self.builder_signer = None;
        self.evm_env.block_env.beneficiary = self.attributes.suggested_fee_recipient;
    }

    pub fn timestamp(&self) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(self.attributes.timestamp as i64)
            .expect("Payload attributes timestamp")
    }

    pub fn block(&self) -> u64 {
        self.evm_env.block_env.number
    }

    pub fn coinbase_is_suggested_fee_recipient(&self) -> bool {
        self.evm_env.block_env.beneficiary == self.attributes.suggested_fee_recipient
    }
}

/// This context should be owned by one thread for the duration of the slot.
/// For example, copy of this should be owned by each builder thread, top of block simulation, finalization thread.
///
/// Its important to not reuse this cache from one payload job to another.
///
/// Caches shared between threads should go to BlockBuildingContext.
#[derive(Debug, Clone, Default)]
pub struct ThreadBlockBuildingContext {
    pub cached_reads: LocalCachedReads,
    pub bloom_cache: BloomCache,
    pub tx_root_cache: TransactionRootCache,
    pub root_hash_calculator: SparseTrieLocalCache,
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
    /// Orders are ordered by their origin (bundle/sbundles then mempool) and then by their absolute profit.
    TypeMaxProfit,
    /// Orders are ordered by length 3 (orders length >= 3 first) and then by their absolute profit.
    LengthThreeMaxProfit,
    /// Orders are ordered by length 3 (orders length >= 3 first) and then by their mev gas price.
    LengthThreeMevGasPrice,
}

const MEV_GAS_PRICE_NAME: &str = "mev_gas_price";
const MAX_PROFIT_NAME: &str = "max_profit";
const TYPE_MAX_PROFIT_NAME: &str = "type_max_profit";
const LENGTH_THREE_MAX_PROFIT_NAME: &str = "length_three_max_profit";
const LENGTH_THREE_MEV_GAS_PRICE_NAME: &str = "length_three_mev_gas_price";

impl FromStr for Sorting {
    type Err = eyre::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            MEV_GAS_PRICE_NAME => Ok(Self::MevGasPrice),
            MAX_PROFIT_NAME => Ok(Self::MaxProfit),
            TYPE_MAX_PROFIT_NAME => Ok(Self::TypeMaxProfit),
            LENGTH_THREE_MAX_PROFIT_NAME => Ok(Self::LengthThreeMaxProfit),
            LENGTH_THREE_MEV_GAS_PRICE_NAME => Ok(Self::LengthThreeMevGasPrice),
            _ => eyre::bail!("Invalid algorithm"),
        }
    }
}
impl std::fmt::Display for Sorting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Sorting::MevGasPrice => write!(f, "{}", MEV_GAS_PRICE_NAME),
            Sorting::MaxProfit => write!(f, "{}", MAX_PROFIT_NAME),
            Sorting::TypeMaxProfit => write!(f, "{}", TYPE_MAX_PROFIT_NAME),
            Sorting::LengthThreeMaxProfit => write!(f, "{}", LENGTH_THREE_MAX_PROFIT_NAME),
            Sorting::LengthThreeMevGasPrice => write!(f, "{}", LENGTH_THREE_MEV_GAS_PRICE_NAME),
        }
    }
}

pub trait PartialBlockExecutionTracer: PartialBlockForkExecutionTracer {
    fn update_commit_order_about_to_execute(&mut self, order: &SimulatedOrder);

    fn update_commit_order_executed(
        &mut self,
        order: &SimulatedOrder,
        res: &Result<Result<ExecutionResult, ExecutionError>, CriticalCommitOrderError>,
    );
}
#[derive(Debug, Clone)]
pub struct NullPartialBlockExecutionTracer;
impl PartialBlockExecutionTracer for NullPartialBlockExecutionTracer {
    fn update_commit_order_about_to_execute(&mut self, _order: &SimulatedOrder) {}
    fn update_commit_order_executed(
        &mut self,
        _order: &SimulatedOrder,
        _res: &Result<Result<ExecutionResult, ExecutionError>, CriticalCommitOrderError>,
    ) {
    }
}

impl PartialBlockForkExecutionTracer for NullPartialBlockExecutionTracer {
    fn update_commit_tx_about_to_execute(
        &mut self,
        _tx_with_blobs: &TransactionSignedEcRecoveredWithBlobs,
        _cumulative_gas_used: u64,
        _gas_reserved: u64,
        _cumulative_blob_gas_used: u64,
    ) {
    }
    fn update_commit_tx_executed(
        &mut self,
        _tx_with_blobs: &TransactionSignedEcRecoveredWithBlobs,
        _cumulative_gas_used: u64,
        _gas_reserved: u64,
        _cumulative_blob_gas_used: u64,
        _res: &Result<Result<TransactionOk, TransactionErr>, CriticalCommitOrderError>,
    ) {
    }
}

#[derive(Debug, Clone)]
pub struct PartialBlock<
    Tracer: SimulationTracer,
    PartialBlockExecutionTracerType: PartialBlockExecutionTracer,
> {
    /// Value used as allow_tx_skip on calls to [`PartialBlockFork`]
    pub discard_txs: bool,
    pub gas_used: u64,
    /// Reserved gas for later use (usually final payout tx). When simulating we subtract this from the block gas limit.
    pub gas_reserved: u64,
    pub blob_gas_used: u64,
    /// Updated after each order.
    pub coinbase_profit: U256,
    /// Tx execution info belonging to successfully executed orders.
    pub executed_tx_infos: Vec<TransactionExecutionInfo>,
    /// Combined refunds.
    pub combined_refunds: HashMap<Address, U256>,
    pub tracer: Tracer,
    partial_block_execution_tracer: PartialBlockExecutionTracerType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionResult {
    pub coinbase_profit: U256,
    pub inplace_sim: SimValue,
    pub gas_used: u64,
    pub order: Order,
    pub tx_infos: Vec<TransactionExecutionInfo>,
    /// Patch to get the executed OrderIds for merged sbundles (see: [`BundleOk::original_order_ids`],[`ShareBundleMerger`] )
    /// Fully dropped orders (TxRevertBehavior::AllowedExcluded allows it!) are not included.
    pub original_order_ids: Vec<OrderId>,
    pub nonces_updated: Vec<(Address, u64)>,
    pub paid_kickbacks: Vec<(Address, U256)>,
}

#[derive(Error, Debug)]
pub enum InsertPayoutTxErr {
    #[error("Critical order commit error: {0}")]
    CriticalCommitError(#[from] CriticalCommitOrderError),
    #[error("Profit too low to insert payout tx")]
    ProfitTooLow,
    #[error("Combined refund tx reverted")]
    CombinedRefundTxReverted,
    #[error("Payout tx reverted")]
    PayoutTxReverted,
    #[error("Signer error: {0}")]
    SignerError(#[from] secp256k1::Error),
    #[error("Tx error: {0}")]
    TxErr(#[from] TransactionErr),
    #[error("Payout without signer")]
    NoSigner,
}

#[allow(clippy::large_enum_variant)]
#[derive(Error, Debug, PartialEq, Eq)]
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
    #[error("Block execution error: {0:?}")]
    BlockExecution(#[from] BlockExecutionError),
    #[error("Other error: {0:?}")]
    Other(#[from] eyre::Report),
}

impl FinalizeError {
    /// see `RootHashError::is_consistent_db_view_err`
    pub fn is_consistent_db_view_err(&self) -> bool {
        if let FinalizeError::RootHash(root_hash) = self {
            root_hash.is_consistent_db_view_err()
        } else {
            false
        }
    }
}

impl<Tracer: SimulationTracer, PartialBlockExecutionTracerType: PartialBlockExecutionTracer>
    PartialBlock<Tracer, PartialBlockExecutionTracerType>
{
    pub fn with_tracer<NewTracer: SimulationTracer>(
        self,
        tracer: NewTracer,
    ) -> PartialBlock<NewTracer, PartialBlockExecutionTracerType> {
        PartialBlock {
            discard_txs: self.discard_txs,
            gas_used: self.gas_used,
            gas_reserved: self.gas_reserved,
            blob_gas_used: self.blob_gas_used,
            coinbase_profit: self.coinbase_profit,
            executed_tx_infos: self.executed_tx_infos,
            combined_refunds: self.combined_refunds,
            tracer,
            partial_block_execution_tracer: self.partial_block_execution_tracer,
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
        local_ctx: &mut ThreadBlockBuildingContext,
        state: &mut BlockState,
        result_filter: &dyn Fn(&SimValue) -> Result<(), ExecutionError>,
    ) -> Result<Result<ExecutionResult, ExecutionError>, CriticalCommitOrderError> {
        self.partial_block_execution_tracer
            .update_commit_order_about_to_execute(order);
        let res = self.commit_order_inner(order, ctx, local_ctx, state, result_filter);
        self.partial_block_execution_tracer
            .update_commit_order_executed(order, &res);
        res
    }

    /// result_filter: little hack to allow "cancel" the execution depending no the SimValue result. Ideally it would be nicer to split commit_order
    ///     in 2 parts, one that executes but does not apply (returns state changes) and then another one that applies the changes.
    ///     You can always pass &|_| Ok(()) if you don't need the filter.
    fn commit_order_inner(
        &mut self,
        order: &SimulatedOrder,
        ctx: &BlockBuildingContext,
        local_ctx: &mut ThreadBlockBuildingContext,
        state: &mut BlockState,
        result_filter: &dyn Fn(&SimValue) -> Result<(), ExecutionError>,
    ) -> Result<Result<ExecutionResult, ExecutionError>, CriticalCommitOrderError> {
        if ctx.builder_signer.is_none() && !order.sim_value.paid_kickbacks().is_empty() {
            // Return here to avoid wasting time on a call to fork.commit_order that 99% will fail
            return Ok(Err(ExecutionError::OrderError(OrderErr::Bundle(
                BundleErr::NoSigner,
            ))));
        }

        let mut fork = PartialBlockFork::new_with_execution_tracer(
            state,
            ctx,
            local_ctx,
            &mut self.partial_block_execution_tracer,
        )
        .with_tracer(&mut self.tracer);

        let rollback = fork.rollback_point();
        let exec_result = fork.commit_order(
            &order.order,
            self.gas_used,
            self.gas_reserved,
            self.blob_gas_used,
            self.discard_txs,
            &self.combined_refunds,
        )?;
        let ok_result = match exec_result {
            Ok(ok) => ok,
            Err(err) => {
                return Ok(Err(err.into()));
            }
        };

        let inplace_sim_result =
            create_sim_value(&order.order, &ok_result, &ctx.mempool_tx_detector);

        match result_filter(&inplace_sim_result) {
            Ok(()) => {}
            Err(err) => {
                fork.rollback(rollback);
                return Ok(Err(err));
            }
        }

        self.gas_used += ok_result.gas_used;
        self.blob_gas_used += ok_result.blob_gas_used;
        self.coinbase_profit += ok_result.coinbase_profit;
        self.executed_tx_infos.extend(ok_result.tx_infos.clone());

        // Update combined refunds
        if let Some(DelayedKickback {
            recipient,
            payout_value,
            ..
        }) = ok_result.delayed_kickback
        {
            let entry = self.combined_refunds.entry(recipient);
            if matches!(entry, hash_map::Entry::Vacant(_)) {
                // This is the first refund for the recipient,
                // so we need to reserve the gas for the refund tx.
                self.gas_reserved += 21_000;
            }
            *entry.or_default() += payout_value;
        }

        Ok(Ok(ExecutionResult {
            coinbase_profit: ok_result.coinbase_profit,
            inplace_sim: inplace_sim_result,
            gas_used: ok_result.gas_used,
            order: order.order.clone(),
            tx_infos: ok_result.tx_infos,
            original_order_ids: ok_result.original_order_ids,
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
            .checked_sub(U256::from(gas_limit) * U256::from(ctx.evm_env.block_env.basefee))
            .ok_or(InsertPayoutTxErr::ProfitTooLow)
    }

    /// Inserts payout tx to ctx.attributes.suggested_fee_recipient (should be called at the end of the block)
    /// Returns the paid value (block profit after subtracting the burned basefee of the payout tx)
    pub fn insert_refunds_and_proposer_payout_tx(
        &mut self,
        gas_limit: u64,
        value: U256,
        ctx: &BlockBuildingContext,
        local_ctx: &mut ThreadBlockBuildingContext,
        state: &mut BlockState,
    ) -> Result<(), InsertPayoutTxErr> {
        let builder_signer = ctx
            .builder_signer
            .as_ref()
            .ok_or(InsertPayoutTxErr::NoSigner)?;
        self.free_reserved_gas();
        let mut nonce = state
            .nonce(
                builder_signer.address,
                &ctx.shared_cached_reads,
                &mut local_ctx.cached_reads,
            )
            .map_err(CriticalCommitOrderError::Reth)?;

        let mut fork = PartialBlockFork::new(state, ctx, local_ctx).with_tracer(&mut self.tracer);

        for (refund_recipient, refund_amount) in &self.combined_refunds {
            let refund_recipient_code_hash = fork
                .state
                .code_hash(
                    *refund_recipient,
                    &ctx.shared_cached_reads,
                    &mut fork.local_ctx.cached_reads,
                )
                .map_err(CriticalCommitOrderError::Reth)?;
            if refund_recipient_code_hash != KECCAK_EMPTY {
                error!(%refund_recipient_code_hash, %refund_recipient, %refund_amount, "Refund recipient has code, skipping refund");
                continue;
            }

            let refund_tx = TransactionSignedEcRecoveredWithBlobs::new_no_blobs(create_payout_tx(
                ctx.chain_spec.as_ref(),
                ctx.evm_env.block_env.basefee,
                builder_signer,
                nonce,
                *refund_recipient,
                BASE_TX_GAS,
                *refund_amount,
            )?)
            .unwrap();
            let refund_result =
                fork.commit_tx(&refund_tx, self.gas_used, 0, self.blob_gas_used)??;
            if !refund_result.tx_info.receipt.success {
                return Err(InsertPayoutTxErr::CombinedRefundTxReverted);
            }

            self.gas_used += refund_result.tx_info.gas_used;
            self.blob_gas_used += refund_result.blob_gas_used;
            self.executed_tx_infos.push(refund_result.tx_info);

            nonce += 1;
        }

        let tx = create_payout_tx(
            ctx.chain_spec.as_ref(),
            ctx.evm_env.block_env.basefee,
            builder_signer,
            nonce,
            ctx.attributes.suggested_fee_recipient,
            gas_limit,
            value,
        )?;
        // payout tx has no blobs so it's safe to unwrap
        let tx = TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx).unwrap();
        let exec_result = fork.commit_tx(&tx, self.gas_used, 0, self.blob_gas_used)?;
        let ok_result = exec_result?;
        if !ok_result.tx_info.receipt.success {
            return Err(InsertPayoutTxErr::PayoutTxReverted);
        }

        self.gas_used += ok_result.tx_info.gas_used;
        self.blob_gas_used += ok_result.blob_gas_used;
        self.executed_tx_infos.push(ok_result.tx_info);

        Ok(())
    }

    /// returns (requests, withdrawals_root)
    pub fn process_requests(
        &self,
        state: &mut BlockState,
        ctx: &BlockBuildingContext,
        local_ctx: &mut ThreadBlockBuildingContext,
    ) -> Result<(Option<Requests>, Option<B256>), FinalizeError> {
        let mut db = state.new_db_ref(&ctx.shared_cached_reads, &mut local_ctx.cached_reads);

        // Apply and gather execution requests
        let requests = if ctx
            .chain_spec
            .is_prague_active_at_timestamp(ctx.attributes.timestamp())
        {
            // Collect all EIP-6110 deposits
            let deposit_requests = eip6110::parse_deposits_from_receipts(
                &ctx.chain_spec,
                self.executed_tx_infos.iter().map(|info| &info.receipt),
            )
            .map_err(BlockExecutionError::Validation)?;

            let mut requests = Requests::default();
            if !deposit_requests.is_empty() {
                requests.push_request_with_type(eip6110::DEPOSIT_REQUEST_TYPE, deposit_requests);
            }

            let mut system_caller = SystemCaller::new(ctx.chain_spec.clone());
            let mut evm = EthEvmConfig::new(ctx.chain_spec.clone())
                .evm_with_env(db.as_mut(), ctx.evm_env.clone());
            requests.extend(system_caller.apply_post_execution_changes(&mut evm)?);
            Some(requests)
        } else {
            None
        };

        // Apply withdrawals
        let withdrawals_root = if ctx
            .chain_spec
            .is_shanghai_active_at_timestamp(ctx.attributes.timestamp)
        {
            let mut balance_increments = HashMap::<Address, u128>::default();
            for withdrawal in &ctx.attributes.withdrawals {
                if withdrawal.amount > 0 {
                    *balance_increments.entry(withdrawal.address).or_default() +=
                        withdrawal.amount_wei().to::<u128>();
                }
            }
            db.db()
                .increment_balances(balance_increments)
                .map_err(|_| {
                    BlockExecutionError::Validation(BlockValidationError::IncrementBalanceFailed)
                })?;
            Some(proofs::calculate_withdrawals_root(
                &ctx.attributes.withdrawals,
            ))
        } else {
            None
        };

        db.db().merge_transitions(BundleRetention::Reverts);

        Ok((requests, withdrawals_root))
    }

    /// Mostly based on reth's (v1.2) default_ethereum_payload_builder.
    #[allow(clippy::too_many_arguments)]
    pub fn finalize(
        self,
        mut state: BlockState,
        ctx: &BlockBuildingContext,
        local_ctx: &mut ThreadBlockBuildingContext,
    ) -> Result<FinalizeResult, FinalizeError> {
        let start = Instant::now();

        let step_start = Instant::now();
        let (requests, withdrawals_root) = self.process_requests(&mut state, ctx, local_ctx)?;
        let block_number = ctx.evm_env.block_env.number;

        let request_processsing_time_ms = elapsed_ms(step_start);
        let step_start = Instant::now();

        let requests_hash = requests.as_ref().map(|requests| requests.requests_hash());

        let exec_outcome_time_ms = elapsed_ms(step_start);
        let step_start = Instant::now();

        let (receipts_root, logs_bloom) = calculate_receipt_root_and_block_logs_bloom(
            &mut local_ctx.bloom_cache,
            &self.executed_tx_infos,
            ctx.faster_finalize,
        );

        let bloom_time_ms = elapsed_ms(step_start);
        let step_start = Instant::now();

        // calculate the state root
        let state_root = {
            let (bundle, _) = state.into_parts();
            // we use execution outcome here only for interface compatibility, its just a wrapper around bundle
            let execution_outcome =
                ExecutionOutcome::new(bundle, Vec::new(), block_number, Vec::new());
            ctx.root_hasher.state_root(&execution_outcome, local_ctx)?
        };
        let root_hash_time = step_start.elapsed();

        let root_hash_time_ms = elapsed_ms(step_start);
        let step_start = Instant::now();

        // create the block header
        // let transactions_root = proofs::calculate_transaction_root(&self.executed_tx);
        let transactions_root = calculate_transactions_root(
            &mut local_ctx.tx_root_cache,
            &self.executed_tx_infos,
            ctx.faster_finalize,
        );

        let transactions_root_time_ms = elapsed_ms(step_start);
        let step_start = Instant::now();

        // double check blocked txs
        for tx_with_blob in self.executed_tx_infos.iter().map(|info| &info.tx) {
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
            for tx_with_blob in self.executed_tx_infos.iter().map(|info| &info.tx) {
                if !tx_with_blob.blobs_sidecar.blobs.is_empty() {
                    txs_blob_sidecars.push(tx_with_blob.blobs_sidecar.clone());
                }
            }
            (ctx.excess_blob_gas, Some(self.blob_gas_used))
        } else {
            (None, None)
        };

        let blobs_time_ms = elapsed_ms(step_start);
        let step_start = Instant::now();

        let header = Header {
            parent_hash: ctx.attributes.parent,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: ctx.evm_env.block_env.beneficiary,
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root,
            logs_bloom,
            timestamp: ctx.attributes.timestamp,
            mix_hash: ctx.attributes.prev_randao,
            nonce: BEACON_NONCE.into(),
            base_fee_per_gas: Some(ctx.evm_env.block_env.basefee),
            number: block_number,
            gas_limit: ctx.evm_env.block_env.gas_limit,
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
                    .executed_tx_infos
                    .into_iter()
                    .map(|t| t.tx.into_internal_tx_unsecure().into_inner())
                    .collect(),
                ommers: vec![],
                withdrawals,
            },
        };
        let result = FinalizeResult {
            sealed_block: block.seal_slow(),
            txs_blob_sidecars,
            root_hash_time,
            execution_requests: requests.map(|er| er.take()).unwrap_or_default(),
        };

        let block_seal_time_ms = elapsed_ms(step_start);

        let total_time_ms = elapsed_ms(start);

        trace!(
            total_time_ms,
            exec_outcome_time_ms,
            bloom_time_ms,
            request_processsing_time_ms,
            root_hash_time_ms,
            transactions_root_time_ms,
            blobs_time_ms,
            block_seal_time_ms,
            "Partial block finalized block"
        );

        Ok(result)
    }

    pub fn pre_block_call(
        &mut self,
        ctx: &BlockBuildingContext,
        local_ctx: &mut ThreadBlockBuildingContext,
        state: &mut BlockState,
    ) -> eyre::Result<()> {
        let mut db = state.new_db_ref(&ctx.shared_cached_reads, &mut local_ctx.cached_reads);
        let mut system_caller = SystemCaller::new(ctx.chain_spec.clone());
        let mut evm = EthEvmConfig::new(ctx.chain_spec.clone())
            .evm_with_env(db.as_mut(), ctx.evm_env.clone());
        system_caller
            .apply_beacon_root_contract_call(ctx.attributes.parent_beacon_block_root(), &mut evm)?;
        system_caller.apply_blockhashes_contract_call(ctx.attributes.parent, &mut evm)?;
        db.as_mut().merge_transitions(BundleRetention::Reverts);
        Ok(())
    }
}

impl PartialBlock<(), NullPartialBlockExecutionTracer> {
    pub fn new(discard_txs: bool) -> Self {
        Self {
            discard_txs,
            gas_used: 0,
            gas_reserved: 0,
            blob_gas_used: 0,
            coinbase_profit: U256::ZERO,
            executed_tx_infos: Vec::new(),
            combined_refunds: HashMap::default(),
            tracer: (),
            partial_block_execution_tracer: NullPartialBlockExecutionTracer {},
        }
    }
}

impl<PartialBlockExecutionTracerType: PartialBlockExecutionTracer>
    PartialBlock<(), PartialBlockExecutionTracerType>
{
    pub fn new_with_execution_tracer(
        discard_txs: bool,
        partial_block_execution_tracer: PartialBlockExecutionTracerType,
    ) -> Self {
        Self {
            discard_txs,
            gas_used: 0,
            gas_reserved: 0,
            blob_gas_used: 0,
            coinbase_profit: U256::ZERO,
            executed_tx_infos: Vec::new(),
            combined_refunds: HashMap::default(),
            tracer: (),
            partial_block_execution_tracer,
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

/// Create the sim value from the order_ok.
/// non_mempool_coinbase_profit for s/bundles will filter tx profit.
/// non_mempool_coinbase_profitm for txs is the same as full_coinbase_profit.
pub fn create_sim_value(
    order: &Order,
    order_ok: &OrderOk,
    mempool_detector: &MempoolTxsDetector,
) -> SimValue {
    let non_mempool_coinbase_profit = if let Order::Tx(_) = order {
        // We don't filter for mempool txs.
        order_ok.coinbase_profit
    } else {
        let non_mempool_coinbase_profit = order_ok
            .tx_infos
            .iter()
            .filter(|tx_info| !mempool_detector.is_mempool(&tx_info.tx))
            .map(|tx_info| tx_info.coinbase_profit)
            .sum::<I256>();
        if non_mempool_coinbase_profit.is_zero() || non_mempool_coinbase_profit.is_positive() {
            non_mempool_coinbase_profit.unsigned_abs()
        } else {
            // This could be a bundle which was positive thanks to the inclusion of mempool txs.
            U256::ZERO
        }
    };

    SimValue::new(
        order_ok.coinbase_profit,
        non_mempool_coinbase_profit,
        order_ok.gas_used,
        order_ok.blob_gas_used,
        order_ok.paid_kickbacks.clone(),
    )
}
#[cfg(test)]
mod test {
    use alloy_primitives::I256;

    use crate::{
        live_builder::order_input::mempool_txs_detector::MempoolTxsDetector,
        primitives::{MempoolTx, Order, TestDataGenerator},
    };

    use super::{create_sim_value, OrderOk, TransactionExecutionInfo};

    /// Create a bundle with 2 txs, one from mempool and the other not.
    /// sim_value.non_mempool_profit_info().coinbase_profit() should only sum the profit for the second.
    #[test]
    fn test_create_sim_value_bundle_non_mempool_coinbase_profit() {
        let detector = MempoolTxsDetector::new();
        let mut data_gen = TestDataGenerator::default();
        let tx1 = data_gen.create_tx_with_blobs_nonce(Default::default());
        detector.add_tx(&Order::Tx(MempoolTx {
            tx_with_blobs: tx1.clone(),
        }));
        let tx2 = data_gen.create_tx_with_blobs_nonce(Default::default());
        let profit_1 = I256::unchecked_from(1000);
        let profit_2 = I256::unchecked_from(10000);
        let order_ok = OrderOk {
            coinbase_profit: Default::default(),
            gas_used: Default::default(),
            cumulative_gas_used: Default::default(),
            blob_gas_used: Default::default(),
            cumulative_blob_gas_used: Default::default(),
            tx_infos: vec![
                TransactionExecutionInfo {
                    tx: tx1,
                    receipt: Default::default(),
                    gas_used: Default::default(),
                    coinbase_profit: profit_1,
                },
                TransactionExecutionInfo {
                    tx: tx2,
                    receipt: Default::default(),
                    gas_used: Default::default(),
                    coinbase_profit: profit_2,
                },
            ],
            delayed_kickback: None,
            original_order_ids: Default::default(),
            nonces_updated: Default::default(),
            paid_kickbacks: Default::default(),
            used_state_trace: Default::default(),
        };
        // dummy bundle just to let know create_sim_value this is a bundle.
        let dummy_bundle = Order::Bundle(data_gen.create_bundle(
            Default::default(),
            Default::default(),
            Default::default(),
        ));
        let sim_value = create_sim_value(&dummy_bundle, &order_ok, &detector);
        assert_eq!(
            sim_value.non_mempool_profit_info().coinbase_profit(),
            profit_2.unsigned_abs()
        );
    }

    /// Create a tx from mempool.
    /// sim_value.non_mempool_profit_info().coinbase_profit() should be the same as full_profit_info = tx profit
    #[test]
    fn test_create_sim_value_tx_non_mempool_coinbase_profit() {
        let detector = MempoolTxsDetector::new();
        let mut data_gen = TestDataGenerator::default();
        let tx = data_gen.create_tx_with_blobs_nonce(Default::default());
        let order = Order::Tx(MempoolTx {
            tx_with_blobs: tx.clone(),
        });
        detector.add_tx(&order);
        let profit = I256::unchecked_from(1000);
        let order_ok = OrderOk {
            coinbase_profit: profit.unsigned_abs(),
            gas_used: Default::default(),
            cumulative_gas_used: Default::default(),
            blob_gas_used: Default::default(),
            cumulative_blob_gas_used: Default::default(),
            tx_infos: vec![TransactionExecutionInfo {
                tx,
                receipt: Default::default(),
                gas_used: Default::default(),
                coinbase_profit: profit,
            }],
            delayed_kickback: None,
            original_order_ids: Default::default(),
            nonces_updated: Default::default(),
            paid_kickbacks: Default::default(),
            used_state_trace: Default::default(),
        };
        let sim_value = create_sim_value(&order, &order_ok, &detector);
        assert_eq!(
            sim_value.non_mempool_profit_info().coinbase_profit(),
            profit.unsigned_abs()
        );
        assert_eq!(
            sim_value.non_mempool_profit_info().coinbase_profit(),
            sim_value.full_profit_info().coinbase_profit(),
        );
    }
}
