pub mod base_config;
pub mod block_output;
pub mod building;
pub mod cli;
pub mod config;
pub mod order_input;
pub mod payload_events;
pub mod simulation;
mod watchdog;

use crate::{
    building::{
        builders::{
            BlockBuildingAlgorithm, BlockBuildingAlgorithmInput, UnfinishedBlockBuildingSinkFactory,
        },
        BlockBuildingContext,
    },
    live_builder::{
        building::BlockBuildingPool,
        order_input::{
            order_replacement_manager::OrderReplacementManager, orderpool::OrdersForBlock,
            start_orderpool_jobs, OrderInputConfig,
        },
        simulation::OrderSimulationPool,
        watchdog::spawn_watchdog_thread,
    },
    telemetry::inc_active_slots,
    utils::{error_storage::spawn_error_storage_writer, ProviderFactoryReopener, Signer},
};
use ahash::HashSet;
use alloy_primitives::{Address, B256};
use eyre::Context;
use jsonrpsee::RpcModule;
use payload_events::MevBoostSlotData;
use reth::{
    primitives::Header,
    providers::{HeaderProvider, ProviderFactory},
};
use reth_chainspec::ChainSpec;
use reth_db::database::Database;
use std::{cmp::min, path::PathBuf, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::{
    sync::{broadcast, mpsc},
    task::spawn_blocking,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

/// Time the proposer have to propose a block from the beginning of the slot (https://www.paradigm.xyz/2023/04/mev-boost-ethereum-consensus Slot anatomy)
const SLOT_PROPOSAL_DURATION: std::time::Duration = Duration::from_secs(4);
/// Delta from slot time to get_header dead line. If we can't get the block header before slot_time + BLOCK_HEADER_DEAD_LINE_DELTA we cancel the slot.
/// Careful: It's signed and usually negative since we need de header BEFORE the slot time.
const BLOCK_HEADER_DEAD_LINE_DELTA: time::Duration = time::Duration::milliseconds(-2500);
/// Polling period while trying to get a block header
const GET_BLOCK_HEADER_PERIOD: time::Duration = time::Duration::milliseconds(250);

/// Trait used to trigger a new block building process in the slot.
pub trait SlotSource {
    fn recv_slot_channel(self) -> mpsc::UnboundedReceiver<MevBoostSlotData>;
}

/// Main builder struct.
/// Connects to the CL, get the new slots and builds blocks for each slot.
/// # Usage
/// Create and run()
#[derive(Debug)]
pub struct LiveBuilder<DB, BlocksSourceType: SlotSource> {
    pub watchdog_timeout: Duration,
    pub error_storage_path: PathBuf,
    pub simulation_threads: usize,
    pub order_input_config: OrderInputConfig,
    pub blocks_source: BlocksSourceType,

    pub chain_chain_spec: Arc<ChainSpec>,
    pub provider_factory: ProviderFactoryReopener<DB>,

    pub coinbase_signer: Signer,
    pub extra_data: Vec<u8>,
    pub blocklist: HashSet<Address>,

    pub global_cancellation: CancellationToken,

    pub sink_factory: Box<dyn UnfinishedBlockBuildingSinkFactory>,
    pub builder: Arc<dyn BlockBuildingAlgorithm<DB>>, // doing the Option because there is a fuunction that creates the live_builder without the builder.
    pub extra_rpc: RpcModule<()>,
}

impl<DB: Database + Clone + 'static, BuilderSourceType: SlotSource>
    LiveBuilder<DB, BuilderSourceType>
{
    pub fn with_extra_rpc(self, extra_rpc: RpcModule<()>) -> Self {
        Self { extra_rpc, ..self }
    }

    pub fn with_builder(self, builder: Arc<dyn BlockBuildingAlgorithm<DB>>) -> Self {
        Self { builder, ..self }
    }

    pub async fn run(self) -> eyre::Result<()> {
        info!("Builder block list size: {}", self.blocklist.len(),);
        info!(
            "Builder coinbase address: {:?}",
            self.coinbase_signer.address
        );

        spawn_error_storage_writer(self.error_storage_path, self.global_cancellation.clone())
            .await
            .with_context(|| "Error spawning error storage writer")?;

        let mut inner_jobs_handles = Vec::new();
        let mut payload_events_channel = self.blocks_source.recv_slot_channel();

        let orderpool_subscriber = {
            let (handle, sub) = start_orderpool_jobs(
                self.order_input_config,
                self.provider_factory.clone(),
                self.extra_rpc,
                self.global_cancellation.clone(),
            )
            .await?;
            inner_jobs_handles.push(handle);
            sub
        };

        let order_simulation_pool = {
            OrderSimulationPool::new(
                self.provider_factory.clone(),
                self.simulation_threads,
                self.global_cancellation.clone(),
            )
        };

        let watchdog_sender = spawn_watchdog_thread(self.watchdog_timeout)?;
        let mut sink_factory = self.sink_factory;

        while let Some(payload) = payload_events_channel.recv().await {
            if self.blocklist.contains(&payload.fee_recipient()) {
                warn!(
                    slot = payload.slot(),
                    "Fee recipient is in blocklist: {:?}",
                    payload.fee_recipient()
                );
                continue;
            }
            // see if we can get parent header in a reasonable time

            let time_to_slot = payload.timestamp() - OffsetDateTime::now_utc();
            debug!(
                slot = payload.slot(),
                block = payload.block(),
                ?time_to_slot,
                "Received payload, time till slot timestamp",
            );

            let time_until_slot_end = time_to_slot + SLOT_PROPOSAL_DURATION;
            if time_until_slot_end.is_negative() {
                warn!(
                    slot = payload.slot(),
                    "Slot already ended, skipping block building"
                );
                continue;
            };

            let parent_header = {
                // @Nicer
                let parent_block = payload.parent_block_hash();
                let timestamp = payload.timestamp();
                let provider_factory = self.provider_factory.provider_factory_unchecked();
                match wait_for_block_header(parent_block, timestamp, &provider_factory).await {
                    Ok(header) => header,
                    Err(err) => {
                        warn!("Failed to get parent header for new slot: {:?}", err);
                        continue;
                    }
                }
            };

            {
                let provider_factory = self.provider_factory.clone();
                let block = payload.block();
                match spawn_blocking(move || {
                    provider_factory.check_consistency_and_reopen_if_needed(block)
                })
                .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(err)) => {
                        error!(?err, "Failed to check historical block hashes");
                        // This error is unrecoverable so we restart.
                        break;
                    }
                    Err(err) => {
                        error!(?err, "Failed to join historical block hashes task");
                        continue;
                    }
                }
            }

            debug!(
                slot = payload.slot(),
                block = payload.block(),
                "Got header for slot"
            );

            inc_active_slots();

            let block_ctx = BlockBuildingContext::from_attributes(
                payload.payload_attributes_event.clone(),
                &parent_header,
                self.coinbase_signer.clone(),
                self.chain_chain_spec.clone(),
                self.blocklist.clone(),
                Some(payload.suggested_gas_limit),
                self.extra_data.clone(),
                None,
            );

            // This was done before in block building pool
            {
                let max_time_to_build = time_until_slot_end.try_into().unwrap_or_default();
                let block_cancellation = self.global_cancellation.clone().child_token();

                let cancel = block_cancellation.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(max_time_to_build).await;
                    cancel.cancel();
                });

                let (orders_for_block, sink) = OrdersForBlock::new_with_sink();
                // add OrderReplacementManager to manage replacements and cancellations
                let order_replacement_manager = OrderReplacementManager::new(Box::new(sink));
                // sink removal is automatic via OrderSink::is_alive false
                let _block_sub = orderpool_subscriber.add_sink(
                    block_ctx.block_env.number.to(),
                    Box::new(order_replacement_manager),
                );

                let simulations_for_block = order_simulation_pool.spawn_simulation_job(
                    block_ctx.clone(),
                    orders_for_block,
                    block_cancellation.clone(),
                );

                let (broadcast_input, _) = broadcast::channel(10_000);
                let builder_sink = sink_factory.create_sink(payload, block_cancellation.clone());

                let input = BlockBuildingAlgorithmInput::<DB> {
                    provider_factory: self.provider_factory.provider_factory_unchecked(),
                    ctx: block_ctx,
                    sink: builder_sink,
                    input: broadcast_input.subscribe(),
                    cancel: block_cancellation,
                };

                tokio::spawn(multiplex_job(simulations_for_block.orders, broadcast_input));
                self.builder.build_blocks(input);
            }

            watchdog_sender.try_send(()).unwrap_or_default();
        }

        info!("Builder shutting down");
        self.global_cancellation.cancel();
        for handle in inner_jobs_handles {
            handle
                .await
                .map_err(|err| warn!("Job handle await error: {:?}", err))
                .unwrap_or_default();
        }
        Ok(())
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

/// May fail if we wait too much (see [BLOCK_HEADER_DEAD_LINE_DELTA])
async fn wait_for_block_header<DB: Database>(
    block: B256,
    slot_time: OffsetDateTime,
    provider_factory: &ProviderFactory<DB>,
) -> eyre::Result<Header> {
    let dead_line = slot_time + BLOCK_HEADER_DEAD_LINE_DELTA;
    while OffsetDateTime::now_utc() < dead_line {
        if let Some(header) = provider_factory.header(&block)? {
            return Ok(header);
        } else {
            let time_to_sleep = min(
                dead_line - OffsetDateTime::now_utc(),
                GET_BLOCK_HEADER_PERIOD,
            );
            if time_to_sleep.is_negative() {
                break;
            }
            tokio::time::sleep(time_to_sleep.try_into().unwrap()).await;
        }
    }
    Err(eyre::eyre!("Block header not found"))
}

#[derive(Debug)]
pub struct NullBlockBuildingAlgorithm {}

impl<DB: Database + std::fmt::Debug + Clone + 'static> BlockBuildingAlgorithm<DB>
    for NullBlockBuildingAlgorithm
{
    fn name(&self) -> String {
        "NullBlockBuildingAlgorithm".to_string()
    }

    fn build_blocks(&self, _input: BlockBuildingAlgorithmInput<DB>) {}
}
