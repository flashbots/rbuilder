//! builders is a subprocess that builds a block
pub mod ordering_builder;

use crate::{
    building::{
        tracers::SimulationTracer, BlockBuildingContext, BlockOrders, BlockState, BuiltBlockTrace,
        InsertPayoutTxErr, PartialBlock, SimulatedOrderSink, Sorting,
    },
    live_builder::{
        bidding::{SealInstruction, SlotBidder},
        payload_events::MevBoostSlotData,
        simulation::SimulatedOrderCommand,
    },
    primitives::{AccountNonce, OrderId, SimulatedOrder},
    utils::NonceCache,
};
use ahash::HashSet;
use alloy_primitives::{Address, B256, U256};
use reth::{
    primitives::{BlobTransactionSidecar, SealedBlock},
    providers::ProviderFactory,
    tasks::pool::BlockingTaskPool,
};
use reth_db::database::Database;
use reth_payload_builder::database::CachedReads;
use std::{
    cmp::max,
    sync::{Arc, Mutex},
};
use tokio::sync::{broadcast, broadcast::error::TryRecvError};
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Block we built
#[derive(Debug, Clone)]
pub struct Block {
    pub trace: BuiltBlockTrace,
    pub sealed_block: SealedBlock,
    /// Sidecars for the txs included in SealedBlock
    pub txs_blobs_sidecars: Vec<Arc<BlobTransactionSidecar>>,
    pub builder_name: String,
}

/// Contains the best block so far.
/// Building updates via compare_and_update while relay submitter polls via take_best_block
#[derive(Debug, Clone)]
pub struct BestBlockCell {
    val: Arc<Mutex<Option<Block>>>,
}

impl Default for BestBlockCell {
    fn default() -> Self {
        Self {
            val: Arc::new(Mutex::new(None)),
        }
    }
}

impl BlockBuildingSink for BestBlockCell {
    fn new_block(&self, block: Block) {
        self.compare_and_update(block);
    }
}

impl BestBlockCell {
    pub fn compare_and_update(&self, block: Block) {
        let mut best_block = self.val.lock().unwrap();
        let old_value = best_block
            .as_ref()
            .map(|b| b.trace.bid_value)
            .unwrap_or_default();
        if block.trace.bid_value > old_value {
            *best_block = Some(block);
        }
    }

    pub fn take_best_block(&self) -> Option<Block> {
        self.val.lock().unwrap().take()
    }
}

#[derive(Debug)]
pub struct LiveBuilderInput<DB: Database, SinkType: BlockBuildingSink> {
    pub provider_factory: ProviderFactory<DB>,
    pub root_hash_task_pool: BlockingTaskPool,
    pub ctx: BlockBuildingContext,
    pub input: broadcast::Receiver<SimulatedOrderCommand>,
    pub sink: SinkType,
    pub builder_name: String,
    pub slot_bidder: Arc<dyn SlotBidder>,
    pub cancel: CancellationToken,
    pub sbundle_mergeabe_signers: Vec<Address>,
}

/// Struct that helps reading new orders/cancelations
/// Call consume_next_commands, check the new_commands() and then consume them via apply_new_commands.
/// Call consume_next_cancellations and use cancel_data
#[derive(Debug)]
pub struct OrderConsumer {
    orders: broadcast::Receiver<SimulatedOrderCommand>,
    // consume_next_batch scratchpad
    new_commands: Vec<SimulatedOrderCommand>,
}

impl OrderConsumer {
    pub fn new(orders: broadcast::Receiver<SimulatedOrderCommand>) -> Self {
        Self {
            orders,
            new_commands: Vec::new(),
        }
    }

    /// Returns true if success, on false builder should stop
    /// New commands are accumulatd in self.new_commands
    /// Call apply_new_commands to easily consume them.
    pub fn consume_next_commands(&mut self) -> eyre::Result<bool> {
        for _ in 0..1024 {
            match self.orders.try_recv() {
                Ok(order) => self.new_commands.push(order),
                Err(TryRecvError::Empty) => {
                    break;
                }
                Err(TryRecvError::Closed) => {
                    return Ok(false);
                }
                Err(TryRecvError::Lagged(msg)) => {
                    warn!("Builder thread lagging on sim orders channel: {}", msg);
                    break;
                }
            }
        }
        Ok(true)
    }

    pub fn new_commands(&self) -> &[SimulatedOrderCommand] {
        &self.new_commands
    }

    // Apply insertions and sbundle cancellations on sink
    pub fn apply_new_commands<SinkType: SimulatedOrderSink>(&mut self, sink: &mut SinkType) {
        for order_command in self.new_commands.drain(..) {
            match order_command {
                SimulatedOrderCommand::Simulation(sim_order) => sink.insert_order(sim_order),
                SimulatedOrderCommand::Cancellation(id) => {
                    let _ = sink.remove_order(id);
                }
            };
        }
    }
}

#[derive(Debug)]
pub struct OrderIntakeConsumer<DB> {
    nonce_cache: NonceCache<DB>,

    block_orders: BlockOrders,
    onchain_nonces_updated: HashSet<Address>,

    order_consumer: OrderConsumer,
}

impl<DB: Database + Clone> OrderIntakeConsumer<DB> {
    /// See [`ShareBundleMerger`] for sbundle_merger_selected_signers
    pub fn new(
        provider_factory: ProviderFactory<DB>,
        orders: broadcast::Receiver<SimulatedOrderCommand>,
        parent_block: B256,
        sorting: Sorting,
        sbundle_merger_selected_signers: &[Address],
    ) -> Self {
        let nonce_cache = NonceCache::new(provider_factory, parent_block);

        Self {
            nonce_cache,
            block_orders: BlockOrders::new(sorting, vec![], sbundle_merger_selected_signers),
            onchain_nonces_updated: HashSet::default(),
            order_consumer: OrderConsumer::new(orders),
        }
    }

    /// Returns true if success, on false builder should stop
    pub fn consume_next_batch(&mut self) -> eyre::Result<bool> {
        self.order_consumer.consume_next_commands()?;
        self.update_onchain_nonces()?;

        self.order_consumer
            .apply_new_commands(&mut self.block_orders);
        Ok(true)
    }

    /// Updates block_orders with all the nonce needed for the new orders
    fn update_onchain_nonces(&mut self) -> eyre::Result<bool> {
        let new_orders = self
            .order_consumer
            .new_commands()
            .iter()
            .filter_map(|sc| match sc {
                SimulatedOrderCommand::Simulation(sim_order) => Some(sim_order),
                SimulatedOrderCommand::Cancellation(_) => None,
            });
        let nonce_db_ref = self.nonce_cache.get_ref()?;
        let mut nonces = Vec::new();
        for new_order in new_orders {
            for nonce in new_order.order.nonces() {
                if self.onchain_nonces_updated.contains(&nonce.address) {
                    continue;
                }
                let onchain_nonce = nonce_db_ref.nonce(nonce.address)?;
                nonces.push(AccountNonce {
                    account: nonce.address,
                    nonce: onchain_nonce,
                });
                self.onchain_nonces_updated.insert(nonce.address);
            }
        }
        self.block_orders.update_onchain_nonces(&nonces);
        Ok(true)
    }

    pub fn current_block_orders(&self) -> BlockOrders {
        self.block_orders.clone()
    }

    pub fn remove_orders(
        &mut self,
        orders: impl IntoIterator<Item = OrderId>,
    ) -> Vec<SimulatedOrder> {
        self.block_orders.remove_orders(orders)
    }
}

pub fn finalize_block_execution(
    ctx: &BlockBuildingContext,
    partial_block: &mut PartialBlock<impl SimulationTracer>,
    state: &mut BlockState,
    built_block_trace: &mut BuiltBlockTrace,
    payout_tx_gas: Option<u64>,
    bidder: &dyn SlotBidder,
    fee_recipient_balance_diff: U256,
) -> Result<bool, InsertPayoutTxErr> {
    let (bid_value, true_value) = if let Some(payout_tx_gas) = payout_tx_gas {
        let available_value = partial_block.get_proposer_payout_tx_value(payout_tx_gas, ctx)?;
        let value = match bidder.seal_instruction(available_value) {
            SealInstruction::Value(value) => value,
            SealInstruction::Skip => return Ok(false),
        };
        match partial_block.insert_proposer_payout_tx(payout_tx_gas, value, ctx, state) {
            Ok(()) => (value, available_value),
            Err(InsertPayoutTxErr::ProfitTooLow) => return Ok(false),
            Err(err) => return Err(err),
        }
    } else {
        (partial_block.coinbase_profit, partial_block.coinbase_profit)
    };
    built_block_trace.bid_value = max(bid_value, fee_recipient_balance_diff);
    built_block_trace.true_bid_value = true_value;

    Ok(true)
}

/// Output of the BlockBuildingAlgorithm
pub trait BlockBuildingSink: std::fmt::Debug + Clone + Send + Sync {
    fn new_block(&self, block: Block);
}

#[derive(Debug)]
pub struct BlockBuildingAlgorithmInput<DB: Database, SinkType: BlockBuildingSink> {
    pub provider_factory: ProviderFactory<DB>,
    pub ctx: BlockBuildingContext,
    pub input: broadcast::Receiver<SimulatedOrderCommand>,
    /// output for the blocks
    pub sink: SinkType,
    /// Needed to add the pay to validator tx (the bid!)
    pub slot_bidder: Arc<dyn SlotBidder>,
    pub cancel: CancellationToken,
}

/// Algorithm to build blocks
/// build_blocks should send block to input.sink until  input.cancel is cancelled.
/// slot_bidder should be used to decide how much to bid.
pub trait BlockBuildingAlgorithm<DB: Database, SinkType: BlockBuildingSink>:
    std::fmt::Debug + Send + Sync
{
    fn name(&self) -> String;
    fn build_blocks(&self, input: BlockBuildingAlgorithmInput<DB, SinkType>);
}

/// Factory used to create BlockBuildingSink for builders when we are targeting blocks for slots.
pub trait BuilderSinkFactory {
    type SinkType: BlockBuildingSink + Clone;
    /// # Arguments
    /// slot_bidder: Not always needed but simplifies the design.
    fn create_builder_sink(
        &self,
        slot_data: MevBoostSlotData,
        slot_bidder: Arc<dyn SlotBidder>,
        cancel: CancellationToken,
    ) -> Self::SinkType;
}

/// Basic configuration to run a single block building with a BlockBuildingAlgorithm
pub struct BacktestSimulateBlockInput<'a, DB> {
    pub ctx: BlockBuildingContext,
    pub builder_name: String,
    pub sbundle_mergeabe_signers: Vec<Address>,
    pub sim_orders: &'a Vec<SimulatedOrder>,
    pub provider_factory: ProviderFactory<DB>,
    pub cached_reads: Option<CachedReads>,
}
