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
use tracing::{debug, trace};
use unfinished_block_building_sink_muxer::UnfinishedBlockBuildingSinkMuxer;

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

        let (orders_for_block, sink) = OrdersForBlock::new_with_sink();
        // add OrderReplacementManager to manage replacements and cancellations
        let order_replacement_manager = OrderReplacementManager::new(Box::new(sink));
        // sink removal is automatic via OrderSink::is_alive false
        let _block_sub = self.orderpool_subscriber.add_sink(
            block_ctx.block_env.number.to(),
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

        let block_number = ctx.block_env.number.to::<u64>();

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
    fn insert_order(&mut self, order: SimulatedOrder) {
        self.sender_returned_error |= self
            .sender
            .send(SimulatedOrderCommand::Simulation(order))
            .is_err()
    }

    fn remove_order(&mut self, id: OrderId) -> Option<SimulatedOrder> {
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
