pub mod built_block_cache;

use crate::{
    building::{
        builders::{BlockBuildingAlgorithm, BlockBuildingAlgorithmInput, BuiltBlockIdSource},
        journal::{OrderJournalObserverFactory, SimulatedOrderJournalCommand},
        BlockBuildingContext,
    },
    live_builder::{
        building::built_block_cache::BuiltBlockCache,
        order_flow_tracing::order_flow_tracer_manager::OrderFlowTracerManager,
        order_input::{
            blob_type_order_filter::BlobTypeOrderFilter,
            replaceable_order_sink::ReplaceableOrderSink,
        },
        payload_events::MevBoostSlotData,
        simulation::SlotOrderSimResults,
    },
    provider::StateProviderFactory,
};
use reth_chainspec::EthereumHardforks as _;
use std::{
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

/// Interval for checking if last block still corresponds to the parent of the given block building context
const CHECK_LAST_BLOCK_INTERVAL: Duration = Duration::from_millis(100);
/// Size of channels for sending simulated orders to the builders or root hash prefetching.
const SIMULATED_ORDERS_CHANNEL_CAPACITY: usize = 10_000;

use super::{
    block_output::unfinished_block_processing::UnfinishedBuiltBlocksInputFactory,
    order_input::{
        self, order_replacement_manager::OrderReplacementManager, orderpool::OrdersForBlock,
    },
    payload_events,
    simulation::{OrderSimulationPool, SimulatedOrderCommand},
};

/// Struct to connect the pipeline for block building.
#[derive(Debug)]
pub struct BlockBuildingPool<P> {
    provider: P,
    builders: Vec<Arc<dyn BlockBuildingAlgorithm<P>>>,
    sink_factory: UnfinishedBuiltBlocksInputFactory<P>,
    orderpool_subscriber: order_input::OrderPoolSubscriber,
    order_simulation_pool: OrderSimulationPool<P>,
    run_sparse_trie_prefetcher: bool,
    order_flow_tracer_manager: Box<dyn OrderFlowTracerManager>,
    built_block_id_source: Arc<BuiltBlockIdSource>,
    order_journal_observer_factory: Box<dyn OrderJournalObserverFactory + Send + Sync>,
}

impl<P> BlockBuildingPool<P>
where
    P: StateProviderFactory + Clone + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: P,
        builders: Vec<Arc<dyn BlockBuildingAlgorithm<P>>>,
        sink_factory: UnfinishedBuiltBlocksInputFactory<P>,
        orderpool_subscriber: order_input::OrderPoolSubscriber,
        order_simulation_pool: OrderSimulationPool<P>,
        run_sparse_trie_prefetcher: bool,
        order_flow_tracer_manager: Box<dyn OrderFlowTracerManager>,
        order_journal_observer_factory: Box<dyn OrderJournalObserverFactory + Send + Sync>,
    ) -> Self {
        BlockBuildingPool {
            provider,
            builders,
            sink_factory,
            orderpool_subscriber,
            order_simulation_pool,
            run_sparse_trie_prefetcher,
            order_flow_tracer_manager,
            built_block_id_source: Arc::new(BuiltBlockIdSource::new()),
            order_journal_observer_factory,
        }
    }

    /// Connects OrdersForBlock (source of orders) ->
    /// [Optional] OrderFlowTracerManager provided tracer ->
    /// BlobTypeOrderFilter (filters out Orders with incorrect blobs (pre/post fusaka)) ->
    /// OrderReplacementManager (Handles cancellations and replacements) -> Simulations and calls start_building_job
    pub fn start_block_building(
        &mut self,
        payload: payload_events::MevBoostSlotData,
        block_ctx: BlockBuildingContext,
        global_cancellation: CancellationToken,
        max_time_to_build: Duration,
    ) {
        let block_cancellation = global_cancellation.child_token();
        let cancel = block_cancellation.clone();
        let block = block_ctx.block();
        let payload_id = block_ctx.payload_id;
        tokio::spawn(async move {
            tokio::time::sleep(max_time_to_build).await;
            info!(
                reason = "max_time_to_build",
                block, payload_id, "Cancelling building job"
            );
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

        let blob_type_order_filter: Box<dyn ReplaceableOrderSink> = if block_ctx
            .chain_spec
            .is_osaka_active_at_timestamp(block_ctx.attributes.timestamp)
        {
            Box::new(BlobTypeOrderFilter::new_fusaka(Box::new(
                order_replacement_manager,
            )))
        } else {
            Box::new(BlobTypeOrderFilter::new_pre_fusaka(Box::new(
                order_replacement_manager,
            )))
        };

        // order_flow_tracer_manager may add some extra  ReplaceableOrderSink on the chain.
        let (sim_tracer, order_flow_input) = self
            .order_flow_tracer_manager
            .create_tracers(payload.slot_block_id(), blob_type_order_filter);

        // sink removal is automatic via OrderSink::is_alive false
        let _block_sub = self
            .orderpool_subscriber
            .add_sink(block_ctx.block(), order_flow_input);

        let simulations_for_block = self.order_simulation_pool.spawn_simulation_job(
            block_ctx.clone(),
            orders_for_block,
            block_cancellation.clone(),
            sim_tracer,
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
        mut input: SlotOrderSimResults,
        cancel: CancellationToken,
    ) {
        let order_journal_observer = match self
            .order_journal_observer_factory
            .create_observer(&slot_data)
        {
            Ok(order_journal_observer) => order_journal_observer,
            Err(err) => {
                error!(
                    ?err,
                    "Failed to create order journal observer, cancelling building job"
                );
                cancel.cancel();
                return;
            }
        };
        let built_block_cache = Arc::new(BuiltBlockCache::new());
        let builder_sink =
            self.sink_factory
                .create_sink(slot_data, built_block_cache.clone(), cancel.clone());
        let (broadcast_input, _) = broadcast::channel(SIMULATED_ORDERS_CHANNEL_CAPACITY);

        let (root_hasher_prefetcher_sender, root_hasher_prefetcher_receiver) =
            mpsc::sync_channel::<SimulatedOrderCommand>(SIMULATED_ORDERS_CHANNEL_CAPACITY);

        let block_number = ctx.block();
        for builder in self.builders.iter() {
            let builder_name = builder.name();
            debug!(
                block = block_number,
                payload_id = ctx.payload_id,
                builder_name,
                "Spawning builder job"
            );

            let input = BlockBuildingAlgorithmInput::<P> {
                provider: self.provider.clone(),
                ctx: ctx.clone(),
                input: broadcast_input.subscribe(),
                sink: builder_sink.clone(),
                cancel: cancel.clone(),
                built_block_cache: built_block_cache.clone(),
                built_block_id_source: self.built_block_id_source.clone(),
            };
            let builder = builder.clone();
            tokio::task::spawn_blocking(move || {
                builder.build_blocks(input);
                debug!(
                    block = block_number,
                    payload_id = ctx.payload_id,
                    builder_name,
                    "Stopped builder job"
                );
            });
        }

        if self.run_sparse_trie_prefetcher {
            tokio::task::spawn_blocking(move || {
                ctx.root_hasher
                    .run_prefetcher(root_hasher_prefetcher_receiver);
            });
        }

        thread::spawn(move || {
            let mut next_journal_sequence_number = 0;
            while let Some(input) = input.orders.blocking_recv() {
                let start = Instant::now();
                let stage = match &input {
                    crate::live_builder::simulation::SimulatedOrderCommand::Simulation(_) => {
                        crate::telemetry::ORDERFLOW_STAGE_JOURNAL_STAMPING_ADD
                    }
                    crate::live_builder::simulation::SimulatedOrderCommand::Cancellation(_) => {
                        crate::telemetry::ORDERFLOW_STAGE_JOURNAL_STAMPING_REMOVE
                    }
                };
                // Failing is not critical.
                let _ = root_hasher_prefetcher_sender.try_send(input.clone());
                let journal_command =
                    SimulatedOrderJournalCommand::new(input, next_journal_sequence_number);
                next_journal_sequence_number += 1;
                order_journal_observer.order_delivered(&journal_command);
                // we don't create new subscribers to the broadcast so here we can be sure that err means end of receivers
                let send_result = broadcast_input.send(journal_command);
                crate::telemetry::add_orderflow_command_process_time(stage, start.elapsed());
                if send_result.is_err() {
                    trace!("Cancelling simulated orders send job, destination stopped");
                    return;
                }
            }
            trace!("Cancelling simulated orders send job, source stopped");
        });
    }
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
