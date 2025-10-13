//! builders is a subprocess that builds a block
pub mod block_building_helper;
pub mod block_building_helper_stats_logger;
pub mod mock_block_building_helper;
pub mod ordering_builder;
pub mod parallel_builder;

use crate::{
    building::{BlockBuildingContext, BuiltBlockTrace, SimulatedOrderSink},
    live_builder::{
        block_output::unfinished_block_processing::UnfinishedBuiltBlocksInput,
        building::built_block_cache::BuiltBlockCache, payload_events::InternalPayloadId,
        simulation::SimulatedOrderCommand,
    },
    provider::StateProviderFactory,
    utils::{is_provider_factory_health_error, NonceCache},
};
use ahash::HashSet;
use alloy_eips::eip7594::BlobTransactionSidecarVariant;
use alloy_primitives::{Address, Bytes};
use rbuilder_primitives::{mev_boost::BidAdjustmentData, AccountNonce, OrderId, SimulatedOrder};
use reth::primitives::SealedBlock;
use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{
    broadcast,
    broadcast::error::{RecvError, TryRecvError},
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{simulated_order_command_to_sink, OrderPriority, PrioritizedOrderStore};

/// Block we built
#[derive(Debug, Clone)]
pub struct Block {
    pub builder_name: String,
    pub trace: BuiltBlockTrace,
    pub sealed_block: SealedBlock,
    /// Sidecars for the txs included in SealedBlock
    pub txs_blobs_sidecars: Vec<Arc<BlobTransactionSidecarVariant>>,
    /// The Pectra execution requests for this bid.
    pub execution_requests: Vec<Bytes>,
    /// Bid adjustment data by fee payer address.
    pub bid_adjustments: HashMap<Address, BidAdjustmentData>,
}

/// Id to uniquely identify every block built (unique even among different algorithms).
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub struct BuiltBlockId(pub u64);

impl BuiltBlockId {
    pub const ZERO: Self = Self(0);
}

#[derive(Debug)]
pub struct BuiltBlockIdSource {
    next_id: AtomicU64,
}

impl BuiltBlockIdSource {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(0),
        }
    }
    pub fn get_new_id(&self) -> BuiltBlockId {
        BuiltBlockId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for BuiltBlockIdSource {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct LiveBuilderInput<P> {
    pub provider: P,
    pub ctx: BlockBuildingContext,
    pub input: broadcast::Receiver<SimulatedOrderCommand>,
    pub sink: UnfinishedBuiltBlocksInput,
    pub builder_name: String,
    pub cancel: CancellationToken,
    pub built_block_cache: Arc<BuiltBlockCache>,
    pub built_block_id_source: Arc<BuiltBlockIdSource>,
    pub max_order_execution_duration_warning: Option<Duration>,
}

/// Struct that helps reading new orders/cancellations
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
    /// This method will block until the first command is received
    pub fn blocking_consume_next_commands(&mut self) -> eyre::Result<bool> {
        match self.orders.blocking_recv() {
            Ok(order) => self.new_commands.push(order),
            Err(RecvError::Closed) => {
                return Ok(false);
            }
            Err(RecvError::Lagged(msg)) => {
                warn!(msg, "Builder thread lagging on sim orders channel");
            }
        }
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
                    warn!(msg, "Builder thread lagging on sim orders channel");
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
pub struct OrderIntakeConsumer<OrderPriorityType> {
    nonces: NonceCache,

    block_orders: PrioritizedOrderStore<OrderPriorityType>,
    onchain_nonces_updated: HashSet<Address>,

    order_consumer: OrderConsumer,
}

impl<OrderPriorityType: OrderPriority> OrderIntakeConsumer<OrderPriorityType> {
    /// See [`ShareBundleMerger`] for sbundle_merger_selected_signers
    pub fn new(nonces: NonceCache, orders: broadcast::Receiver<SimulatedOrderCommand>) -> Self {
        Self {
            nonces,
            block_orders: PrioritizedOrderStore::new(vec![]),
            onchain_nonces_updated: HashSet::default(),
            order_consumer: OrderConsumer::new(orders),
        }
    }

    /// Returns true if success, on false builder should stop
    /// Blocks until the first item in the next batch is available.
    pub fn blocking_consume_next_batch(&mut self) -> eyre::Result<bool> {
        if !self.order_consumer.blocking_consume_next_commands()? {
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
        let mut nonces = Vec::new();
        for new_order in new_orders {
            for nonce in new_order.order.nonces() {
                if self.onchain_nonces_updated.contains(&nonce.address) {
                    continue;
                }
                let onchain_nonce = self.nonces.nonce(nonce.address)?;
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

    pub fn current_block_orders(&self) -> PrioritizedOrderStore<OrderPriorityType> {
        self.block_orders.clone()
    }

    pub fn remove_orders(
        &mut self,
        orders: impl IntoIterator<Item = OrderId>,
    ) -> Vec<Arc<SimulatedOrder>> {
        self.block_orders.remove_orders(orders)
    }
}

#[derive(Debug)]
pub struct BlockBuildingAlgorithmInput<P> {
    pub provider: P,
    pub ctx: BlockBuildingContext,
    pub input: broadcast::Receiver<SimulatedOrderCommand>,
    /// output for the blocks
    pub sink: UnfinishedBuiltBlocksInput,
    /// A cache common to several builders so they can optimize their work looking at other builders blocks.
    pub built_block_cache: Arc<BuiltBlockCache>,
    pub cancel: CancellationToken,
    pub built_block_id_source: Arc<BuiltBlockIdSource>,
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

/// Basic configuration to run a single block building with a BlockBuildingAlgorithm
pub struct BacktestSimulateBlockInput<'a, P> {
    pub ctx: BlockBuildingContext,
    pub builder_name: String,
    pub sim_orders: &'a Vec<Arc<SimulatedOrder>>,
    pub provider: P,
}

/// Handles error from block filling stage.
/// Answers if block filling should continue.
pub fn handle_building_error(err: eyre::Report, payload_id: InternalPayloadId) -> bool {
    // @Types
    let err_str = err.to_string();
    if !err_str.contains("Profit too low") {
        if is_provider_factory_health_error(&err) {
            info!(
                payload_id,
                ?err,
                "Cancelling building due to provider factory error"
            );
            return false;
        } else {
            warn!(?err, "Error filling orders");
        }
    }
    true
}
