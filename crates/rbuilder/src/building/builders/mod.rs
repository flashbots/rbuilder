//! builders is a subprocess that builds a block
pub mod block_building_helper;
pub mod mock_block_building_helper;
pub mod ordering_builder;
pub mod parallel_builder;

use crate::{
    building::{BlockBuildingContext, BuiltBlockTrace, SimulatedOrderSink, Sorting},
    live_builder::{payload_events::MevBoostSlotData, simulation::SimulatedOrderCommand},
    primitives::{AccountNonce, OrderId, SimulatedOrder},
    provider::StateProviderFactory,
    utils::{is_provider_factory_health_error, NonceCache},
};
use ahash::HashSet;
use alloy_eips::eip4844::BlobTransactionSidecar;
use alloy_primitives::{Address, Bytes, B256};
use block_building_helper::BlockBuildingHelper;
use reth::{primitives::SealedBlock, revm::cached::CachedReads};
use reth_errors::ProviderError;
use std::{fmt::Debug, sync::Arc};
use tokio::sync::{broadcast, broadcast::error::TryRecvError};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{simulated_order_command_to_sink, PrioritizedOrderStore};

/// Block we built
#[derive(Debug, Clone)]
pub struct Block {
    pub trace: BuiltBlockTrace,
    pub sealed_block: SealedBlock,
    /// Sidecars for the txs included in SealedBlock
    pub txs_blobs_sidecars: Vec<Arc<BlobTransactionSidecar>>,
    /// The Pectra execution requests for this bid.
    pub execution_requests: Vec<Bytes>,
    pub builder_name: String,
}

#[derive(Debug)]
pub struct LiveBuilderInput<P> {
    pub provider: P,
    pub ctx: BlockBuildingContext,
    pub input: broadcast::Receiver<SimulatedOrderCommand>,
    pub sink: Arc<dyn UnfinishedBlockBuildingSink>,
    pub builder_name: String,
    pub cancel: CancellationToken,
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
            simulated_order_command_to_sink(order_command, sink);
        }
    }
}

#[derive(Debug)]
pub struct OrderIntakeConsumer<P> {
    nonce_cache: NonceCache<P>,

    block_orders: PrioritizedOrderStore,
    onchain_nonces_updated: HashSet<Address>,

    order_consumer: OrderConsumer,
}

impl<P> OrderIntakeConsumer<P>
where
    P: StateProviderFactory,
{
    /// See [`ShareBundleMerger`] for sbundle_merger_selected_signers
    pub fn new(
        provider: P,
        orders: broadcast::Receiver<SimulatedOrderCommand>,
        parent_block: B256,
        sorting: Sorting,
    ) -> Self {
        let nonce_cache = NonceCache::new(provider, parent_block);

        Self {
            nonce_cache,
            block_orders: PrioritizedOrderStore::new(sorting, vec![]),
            onchain_nonces_updated: HashSet::default(),
            order_consumer: OrderConsumer::new(orders),
        }
    }

    /// Returns true if success, on false builder should stop
    pub fn consume_next_batch(&mut self) -> eyre::Result<bool> {
        if !self.order_consumer.consume_next_commands()? {
            return Ok(false);
        }
        if !self.update_onchain_nonces()? {
            return Ok(false);
        }

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
        let nonce_db_ref = match self.nonce_cache.get_ref() {
            Ok(nonce_db_ref) => nonce_db_ref,
            Err(ProviderError::BlockHashNotFound(_)) => return Ok(false), // This can happen on reorgs since the block is removed
            Err(err) => return Err(err.into()),
        };
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

    pub fn current_block_orders(&self) -> PrioritizedOrderStore {
        self.block_orders.clone()
    }

    pub fn remove_orders(
        &mut self,
        orders: impl IntoIterator<Item = OrderId>,
    ) -> Vec<SimulatedOrder> {
        self.block_orders.remove_orders(orders)
    }
}

/// Output of the BlockBuildingAlgorithm.
pub trait UnfinishedBlockBuildingSink: std::fmt::Debug + Send + Sync {
    fn new_block(&self, block: Box<dyn BlockBuildingHelper>);

    /// The sink may not like blocks where coinbase is the final fee_recipient (eg: this does not allows us to take profit!).
    /// Not sure this is the right place for this func. Might move somewhere else.
    fn can_use_suggested_fee_recipient_as_coinbase(&self) -> bool;
}

#[derive(Debug)]
pub struct BlockBuildingAlgorithmInput<P> {
    pub provider: P,
    pub ctx: BlockBuildingContext,
    pub input: broadcast::Receiver<SimulatedOrderCommand>,
    /// output for the blocks
    pub sink: Arc<dyn UnfinishedBlockBuildingSink>,
    pub cancel: CancellationToken,
}

/// Algorithm to build blocks
/// build_blocks should send block to input.sink until  input.cancel is cancelled.
/// slot_bidder should be used to decide how much to bid.
pub trait BlockBuildingAlgorithm<P>: Debug + Send + Sync
where
    P: StateProviderFactory,
{
    fn name(&self) -> String;
    fn build_blocks(&self, input: BlockBuildingAlgorithmInput<P>);
}

/// Factory used to create UnfinishedBlockBuildingSink for builders.
pub trait UnfinishedBlockBuildingSinkFactory: Debug + Send + Sync {
    /// Creates an UnfinishedBlockBuildingSink to receive block for slot_data.
    /// cancel: If this is signaled the sink should cancel. If any unrecoverable situation is found signal cancel.
    fn create_sink(
        &mut self,
        slot_data: MevBoostSlotData,
        cancel: CancellationToken,
    ) -> Arc<dyn UnfinishedBlockBuildingSink>;
}

/// Basic configuration to run a single block building with a BlockBuildingAlgorithm
pub struct BacktestSimulateBlockInput<'a, P> {
    pub ctx: BlockBuildingContext,
    pub builder_name: String,
    pub sim_orders: &'a Vec<SimulatedOrder>,
    pub provider: P,
    pub cached_reads: Option<CachedReads>,
}

/// Handles error from block filling stage.
/// Answers if block filling should continue.
pub fn handle_building_error(err: eyre::Report) -> bool {
    // @Types
    let err_str = err.to_string();
    if !err_str.contains("Profit too low") {
        if is_provider_factory_health_error(&err) {
            info!(?err, "Cancelling building due to provider factory error");
            return false;
        } else {
            warn!(?err, "Error filling orders");
        }
    }
    true
}
