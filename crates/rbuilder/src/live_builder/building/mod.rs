use std::{sync::Arc, time::Duration};

use crate::{
    building::{
        builders::{
            BlockBuildingAlgorithm, BlockBuildingAlgorithmInput, UnfinishedBlockBuildingSinkFactory,
            UnfinishedBlockBuildingSink,
            bob_builder::run_bob_builder,
        },
        BlockBuildingContext,
    },
    live_builder::{simulation::SlotOrderSimResults},
    live_builder::block_output::block_router::UnfinishedBlockRouter,
    live_builder::streaming::block_subscription_server::start_block_subscription_server,
    utils::ProviderFactoryReopener,
};
use reth_db::database::Database;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, trace};

use super::{
    order_input::{
        self,
        bob_order_shim::BobOrderShim,
        order_replacement_manager::OrderReplacementManager,
        orderpool::OrdersForBlock,
    },
    payload_events,
    simulation::OrderSimulationPool,
};

#[derive(Debug)]
pub struct BlockBuildingPool<DB> {
    provider_factory: ProviderFactoryReopener<DB>,
    builders: Vec<Arc<dyn BlockBuildingAlgorithm<DB>>>,
    sink_factory: Box<dyn UnfinishedBlockBuildingSinkFactory>,
    orderpool_subscriber: order_input::OrderPoolSubscriber,
    order_simulation_pool: OrderSimulationPool<DB>,
    state_diff_server: broadcast::Sender<serde_json::Value>,
}

impl<DB: Database + Clone + 'static> BlockBuildingPool<DB> {
    pub async fn new(
        provider_factory: ProviderFactoryReopener<DB>,
        builders: Vec<Arc<dyn BlockBuildingAlgorithm<DB>>>,
        sink_factory: Box<dyn UnfinishedBlockBuildingSinkFactory>,
        orderpool_subscriber: order_input::OrderPoolSubscriber,
        order_simulation_pool: OrderSimulationPool<DB>,
    ) -> Self {
        let state_diff_server = start_block_subscription_server().await.expect("Failed to start block subscription server");
        BlockBuildingPool {
            provider_factory,
            builders,
            sink_factory,
            orderpool_subscriber,
            order_simulation_pool,
            state_diff_server,
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
            true,
        );

        let (bob_order_shim, bob_order_receiver) = BobOrderShim::new();
        self.orderpool_subscriber.add_sink(
            block_ctx.block_env.number.to::<u64>(),
            Box::new(bob_order_shim),
            false,
        );

        let slot_timestamp = payload.timestamp();
        let (block_router, block_receiver) = UnfinishedBlockRouter::new(
            self.sink_factory.create_sink(payload, block_cancellation.clone()),
            self.state_diff_server.clone(),
            slot_timestamp,
        );
        let block_router = Arc::new(block_router);

        let simulations_for_block = self.order_simulation_pool.spawn_simulation_job(
            block_ctx.clone(),
            orders_for_block,
            block_cancellation.clone(),
        );
        self.start_building_job(
            block_ctx,
            block_router.clone(),
            simulations_for_block,
            block_cancellation.clone(),
        );
        tokio::spawn(run_bob_builder(
            block_router,
            bob_order_receiver,
            block_receiver,
            block_cancellation.clone(),
        ));
    }

    /// Per each BlockBuildingAlgorithm creates BlockBuildingAlgorithmInput and Sinks and spawn a task to run it
    fn start_building_job(
        &mut self,
        ctx: BlockBuildingContext,
        builder_sink: Arc<dyn UnfinishedBlockBuildingSink>,
        input: SlotOrderSimResults,
        cancel: CancellationToken,
    ) {
        let (broadcast_input, _) = broadcast::channel(10_000);

        let block_number = ctx.block_env.number.to::<u64>();
        let provider_factory = match self
            .provider_factory
            .check_consistency_and_reopen_if_needed(block_number)
        {
            Ok(provider_factory) => provider_factory,
            Err(err) => {
                error!(?err, "Error while reopening provider factory");
                return;
            }
        };

        for builder in self.builders.iter() {
            let builder_name = builder.name();
            debug!(block = block_number, builder_name, "Spawning builder job");
            let input = BlockBuildingAlgorithmInput::<DB> {
                provider_factory: provider_factory.clone(),
                ctx: ctx.clone(),
                input: broadcast_input.subscribe(),
                sink: builder_sink.clone(),
                cancel: cancel.clone(),
            };
            let builder = builder.clone();
            tokio::task::spawn_blocking(move || {
                builder.build_blocks(input);
                debug!(block = block_number, builder_name, "Stopped builder job");
            });
        }

        tokio::spawn(multiplex_job(input.orders, broadcast_input));
    }
}

async fn multiplex_job<T>(mut input: mpsc::Receiver<T>, sender: broadcast::Sender<T>) {
    // we don't worry about waiting for input forever because it will be closed by producer job
    while let Some(input) = input.recv().await {
        // we don't create new subscribers to the broadcast so here we can be sure that err means end of receivers
        if sender.send(input).is_err() {
            return;
        }
    }
    trace!("Cancelling multiplex job");
}
