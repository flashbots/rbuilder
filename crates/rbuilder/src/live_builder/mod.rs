pub mod base_config;
pub mod bidding;
pub mod building;
pub mod cli;
pub mod config;
pub mod order_input;
pub mod payload_events;
pub mod simulation;
mod watchdog;

use crate::{
    building::{
        builders::{BlockBuildingAlgorithm, BuilderSinkFactory},
        BlockBuildingContext,
    },
    live_builder::{
        order_input::{start_orderpool_jobs, OrderInputConfig},
        simulation::OrderSimulationPool,
        watchdog::spawn_watchdog_thread,
    },
    telemetry::inc_active_slots,
    utils::{error_storage::spawn_error_storage_writer, ProviderFactoryReopener, Signer},
};
use ahash::HashSet;
use alloy_primitives::{Address, B256};
use bidding::BiddingService;
use building::BlockBuildingPool;
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
use tokio::{sync::mpsc, task::spawn_blocking};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

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
pub struct LiveBuilder<DB, BuilderSinkFactoryType: BuilderSinkFactory, BlocksSourceType: SlotSource>
{
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

    pub bidding_service: Box<dyn BiddingService>,

    pub sink_factory: BuilderSinkFactoryType,
    pub builders: Vec<Arc<dyn BlockBuildingAlgorithm<DB, BuilderSinkFactoryType::SinkType>>>,
    pub extra_rpc: RpcModule<()>,
}

impl<
        DB: Database + Clone + 'static,
        BuilderSinkFactoryType: BuilderSinkFactory,
        BuilderSourceType: SlotSource,
    > LiveBuilder<DB, BuilderSinkFactoryType, BuilderSourceType>
where
    <BuilderSinkFactoryType as BuilderSinkFactory>::SinkType: 'static,
{
    pub fn with_bidding_service(self, bidding_service: Box<dyn BiddingService>) -> Self {
        Self {
            bidding_service,
            ..self
        }
    }

    pub fn with_extra_rpc(self, extra_rpc: RpcModule<()>) -> Self {
        Self { extra_rpc, ..self }
    }

    pub fn with_builders(
        self,
        builders: Vec<Arc<dyn BlockBuildingAlgorithm<DB, BuilderSinkFactoryType::SinkType>>>,
    ) -> Self {
        Self { builders, ..self }
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

        let mut builder_pool = BlockBuildingPool::new(
            self.provider_factory.clone(),
            self.builders,
            self.sink_factory,
            self.bidding_service,
            orderpool_subscriber,
            order_simulation_pool,
        );

        let watchdog_sender = spawn_watchdog_thread(self.watchdog_timeout)?;

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

            builder_pool.start_block_building(
                payload,
                block_ctx,
                self.global_cancellation.clone(),
                time_until_slot_end.try_into().unwrap_or_default(),
            );

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
