mod unfinished_block_building_sink_muxer;

use std::{cell::RefCell, rc::Rc, sync::Arc, thread, time::Duration};

use crate::{
    building::{
        builders::{
            BlockBuildingAlgorithm, BlockBuildingAlgorithmInput, UnfinishedBlockBuildingSinkFactory,
        },
        multi_share_bundle_merger::MultiShareBundleMerger,
        simulated_order_command_to_sink, BlockBuildingContext, SimulatedOrderSink,
    },
    live_builder::{payload_events::MevBoostSlotData, simulation::SlotOrderSimResults},
    primitives::{OrderId, SimulatedOrder},
    provider::StateProviderFactory,
};
use revm_primitives::Address;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};
use unfinished_block_building_sink_muxer::UnfinishedBlockBuildingSinkMuxer;

/// Interval for checking if last block still corresponds to the parent of the given block building context
const CHECK_LAST_BLOCK_INTERVAL: Duration = Duration::from_millis(100);

use super::{
    order_input::{
        self, order_replacement_manager::OrderReplacementManager, orderpool::OrdersForBlock,
    },
    payload_events,
    simulation::{OrderSimulationPool, SimulatedOrderCommand},
};

#[derive(Debug)]
pub struct BlockBuildingPool<P> {
    provider: P,
    builders: Vec<Arc<dyn BlockBuildingAlgorithm<P>>>,
    sink_factory: Box<dyn UnfinishedBlockBuildingSinkFactory>,
    orderpool_subscriber: order_input::OrderPoolSubscriber,
    order_simulation_pool: OrderSimulationPool<P>,
    run_sparse_trie_prefetcher: bool,
    sbundle_merger_selected_signers: Arc<Vec<Address>>,
}

impl<P> BlockBuildingPool<P>
where
    P: StateProviderFactory + Clone + 'static,
{
    pub fn new(
        provider: P,
        builders: Vec<Arc<dyn BlockBuildingAlgorithm<P>>>,
        sink_factory: Box<dyn UnfinishedBlockBuildingSinkFactory>,
        orderpool_subscriber: order_input::OrderPoolSubscriber,
        order_simulation_pool: OrderSimulationPool<P>,
        run_sparse_trie_prefetcher: bool,
        sbundle_merger_selected_signers: Arc<Vec<Address>>,
    ) -> Self {
        BlockBuildingPool {
            provider,
            builders,
            sink_factory,
            orderpool_subscriber,
            order_simulation_pool,
            run_sparse_trie_prefetcher,
            sbundle_merger_selected_signers,
        }
    }

    /// Connects OrdersForBlock->OrderReplacementManager->Simulations and calls start_building_job
    pub fn start_block_building(
        &mut self,
        payload: payload_events::MevBoostSlotData,
        block_ctx: BlockBuildingContext,
        global_cancellation: CancellationToken,
        max_time_to_build: Duration,
    ) {
        let block_cancellation = global_cancellation.child_token();

        let cancel = block_cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(max_time_to_build).await;
            cancel.cancel();
        });

        {
            let provider = self.provider.clone();
            let block_ctx = block_ctx.clone();
            let block_cancellation = block_cancellation.clone();
            tokio::task::spawn_blocking(move || {
                run_check_if_parent_block_is_last_block(provider, block_ctx, block_cancellation);
            });
        }

        let (orders_for_block, sink) = OrdersForBlock::new_with_sink();
        // add OrderReplacementManager to manage replacements and cancellations
        let order_replacement_manager = OrderReplacementManager::new(Box::new(sink));
        // sink removal is automatic via OrderSink::is_alive false
        let _block_sub = self.orderpool_subscriber.add_sink(
            block_ctx.evm_env.block_env.number.to(),
            Box::new(order_replacement_manager),
        );

        let simulations_for_block = self.order_simulation_pool.spawn_simulation_job(
            block_ctx.clone(),
            orders_for_block,
            block_cancellation.clone(),
        );
        self.start_building_job(
            block_ctx,
            payload,
            simulations_for_block,
            block_cancellation,
        );
    }

    /// Per each BlockBuildingAlgorithm creates BlockBuildingAlgorithmInput and Sinks and spawn a task to run it
    fn start_building_job(
        &mut self,
        ctx: BlockBuildingContext,
        slot_data: MevBoostSlotData,
        input: SlotOrderSimResults,
        cancel: CancellationToken,
    ) {
        let builder_sink = self.sink_factory.create_sink(slot_data, cancel.clone());
        let (broadcast_input, _) = broadcast::channel(10_000);
        let muxer = Arc::new(UnfinishedBlockBuildingSinkMuxer::new(builder_sink));

        let block_number = ctx.evm_env.block_env.number.to::<u64>();

        for builder in self.builders.iter() {
            let builder_name = builder.name();
            debug!(block = block_number, builder_name, "Spawning builder job");
            let input = BlockBuildingAlgorithmInput::<P> {
                provider: self.provider.clone(),
                ctx: ctx.clone(),
                input: broadcast_input.subscribe(),
                sink: muxer.clone(),
                cancel: cancel.clone(),
            };
            let builder = builder.clone();
            tokio::task::spawn_blocking(move || {
                builder.build_blocks(input);
                debug!(block = block_number, builder_name, "Stopped builder job");
            });
        }

        if self.run_sparse_trie_prefetcher {
            let input = broadcast_input.subscribe();

            tokio::task::spawn_blocking(move || {
                ctx.root_hasher.run_prefetcher(input, cancel);
            });
        }

        let sbundle_merger_selected_signers = self.sbundle_merger_selected_signers.clone();
        thread::spawn(move || {
            merge_and_send(
                input.orders,
                broadcast_input,
                &sbundle_merger_selected_signers,
            )
        });

        //        tokio::spawn();
    }
}

/// Implements SimulatedOrderSink and sends everything to a broadcast::Sender as SimulatedOrderCommand.
struct SimulatedOrderSinkToChannel {
    sender: broadcast::Sender<SimulatedOrderCommand>,
    sender_returned_error: bool,
}

impl SimulatedOrderSinkToChannel {
    pub fn new(sender: broadcast::Sender<SimulatedOrderCommand>) -> Self {
        Self {
            sender,
            sender_returned_error: false,
        }
    }

    pub fn sender_returned_error(&self) -> bool {
        self.sender_returned_error
    }
}

impl SimulatedOrderSink for SimulatedOrderSinkToChannel {
    fn insert_order(&mut self, order: Arc<SimulatedOrder>) {
        self.sender_returned_error |= self
            .sender
            .send(SimulatedOrderCommand::Simulation(order))
            .is_err()
    }

    fn remove_order(&mut self, id: OrderId) -> Option<Arc<SimulatedOrder>> {
        self.sender_returned_error |= self
            .sender
            .send(SimulatedOrderCommand::Cancellation(id))
            .is_err();
        None
    }
}

/// Merges (see [`MultiShareBundleMerger`]) simulated orders from input and forwards the result to sender.
fn merge_and_send(
    mut input: mpsc::Receiver<SimulatedOrderCommand>,
    sender: broadcast::Sender<SimulatedOrderCommand>,
    sbundle_merger_selected_signers: &[Address],
) {
    let sender = Rc::new(RefCell::new(SimulatedOrderSinkToChannel::new(sender)));
    let mut merger = MultiShareBundleMerger::new(sbundle_merger_selected_signers, sender.clone());
    // we don't worry about waiting for input forever because it will be closed by producer job
    while let Some(input) = input.blocking_recv() {
        simulated_order_command_to_sink(input, &mut merger);
        // we don't create new subscribers to the broadcast so here we can be sure that err means end of receivers
        if sender.borrow().sender_returned_error() {
            trace!("Cancelling merge_and_send job, destination stopped");
            return;
        }
    }
    trace!("Cancelling merge_and_send job, source stopped");
}

fn run_check_if_parent_block_is_last_block<P>(
    provider: P,
    block_ctx: BlockBuildingContext,
    block_cancellation: CancellationToken,
) where
    P: StateProviderFactory + Clone + 'static,
{
    loop {
        std::thread::sleep(CHECK_LAST_BLOCK_INTERVAL);
        if block_cancellation.is_cancelled() {
            return;
        }
        let last_block_number = match provider.last_block_number() {
            Ok(n) => n,
            Err(err) => {
                warn!(?err, "Failed to get last block number");
                continue;
            }
        };
        if last_block_number + 1 != block_ctx.block() {
            info!(
                reason = "last block number",
                last_block_number,
                block = block_ctx.block(),
                payload_id = block_ctx.payload_id,
                "Cancelling building job"
            );
            block_cancellation.cancel();
            return;
        }

        let last_block_hash = match provider.block_hash(last_block_number) {
            Ok(Some(h)) => h,
            Ok(None) => {
                warn!(err = "hash is missing", "Failed to get last block hash");
                continue;
            }
            Err(err) => {
                warn!(?err, "Failed to get last block hash");
                continue;
            }
        };

        let parent_hash = block_ctx.attributes.parent;
        if last_block_hash != parent_hash {
            info!(
                reason = "last block hash",
                ?last_block_hash,
                ?parent_hash,
                block = block_ctx.block(),
                payload_id = block_ctx.payload_id,
                "Cancelling building job"
            );
            block_cancellation.cancel();
            return;
        }
    }
}
