pub mod base_config;
pub mod block_output;
pub mod building;
pub mod cli;
pub mod config;
pub mod order_input;
pub mod payload_events;
pub mod simulation;
pub mod watchdog;

use crate::{
    building::{
        builders::{BlockBuildingAlgorithm, UnfinishedBlockBuildingSinkFactory},
        BlockBuildingContext,
    },
    live_builder::{
        order_input::{start_orderpool_jobs, OrderInputConfig},
        simulation::OrderSimulationPool,
        watchdog::spawn_watchdog_thread,
    },
    telemetry::inc_active_slots,
    utils::{error_storage::spawn_error_storage_writer, Signer},
};
use ahash::HashSet;
use alloy_primitives::{Address, B256};
use building::BlockBuildingPool;
use eyre::Context;
use jsonrpsee::RpcModule;
use payload_events::MevBoostSlotData;
use reth::{primitives::Header, providers::HeaderProvider};
use reth_chainspec::ChainSpec;
use reth_db::Database;
use reth_provider::{DatabaseProviderFactory, StateProviderFactory};
use std::fmt::Debug;
use std::{cmp::min, path::PathBuf, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

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
pub struct LiveBuilder<P, DB, BlocksSourceType>
where
    DB: Database + Clone + 'static,
    P: StateProviderFactory + Clone,
    BlocksSourceType: SlotSource,
{
    pub watchdog_timeout: Duration,
    pub error_storage_path: Option<PathBuf>,
    pub simulation_threads: usize,
    pub order_input_config: OrderInputConfig,
    pub blocks_source: BlocksSourceType,
    pub run_sparse_trie_prefetcher: bool,

    pub chain_chain_spec: Arc<ChainSpec>,
    pub provider: P,

    pub coinbase_signer: Signer,
    pub extra_data: Vec<u8>,
    pub blocklist: HashSet<Address>,

    pub global_cancellation: CancellationToken,

    pub sink_factory: Box<dyn UnfinishedBlockBuildingSinkFactory>,
    pub builders: Vec<Arc<dyn BlockBuildingAlgorithm<P, DB>>>,
    pub extra_rpc: RpcModule<()>,
}

impl<P, DB, BlocksSourceType: SlotSource> LiveBuilder<P, DB, BlocksSourceType>
where
    DB: Database + Clone + 'static,
    P: DatabaseProviderFactory<DB> + StateProviderFactory + HeaderProvider + Clone + 'static,
    BlocksSourceType: SlotSource,
{
    pub fn with_extra_rpc(self, extra_rpc: RpcModule<()>) -> Self {
        Self { extra_rpc, ..self }
    }

    pub fn with_builders(self, builders: Vec<Arc<dyn BlockBuildingAlgorithm<P, DB>>>) -> Self {
        Self { builders, ..self }
    }

    pub async fn run(self) -> eyre::Result<()> {
        info!("Builder block list size: {}", self.blocklist.len(),);
        info!(
            "Builder coinbase address: {:?}",
            self.coinbase_signer.address
        );

        if let Some(error_storage_path) = self.error_storage_path {
            spawn_error_storage_writer(error_storage_path, self.global_cancellation.clone())
                .await
                .with_context(|| "Error spawning error storage writer")?;
        }

        let mut inner_jobs_handles = Vec::new();
        let mut payload_events_channel = self.blocks_source.recv_slot_channel();

        let orderpool_subscriber = {
            let (handle, sub) = start_orderpool_jobs(
                self.order_input_config,
                self.provider.clone(),
                self.extra_rpc,
                self.global_cancellation.clone(),
            )
            .await?;
            inner_jobs_handles.push(handle);
            sub
        };

        let order_simulation_pool = {
            OrderSimulationPool::new(
                self.provider.clone(),
                self.simulation_threads,
                self.global_cancellation.clone(),
            )
        };

        let mut builder_pool = BlockBuildingPool::new(
            self.provider.clone(),
            self.builders,
            self.sink_factory,
            orderpool_subscriber,
            order_simulation_pool,
            self.run_sparse_trie_prefetcher,
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
                match wait_for_block_header(parent_block, timestamp, &self.provider).await {
                    Ok(header) => header,
                    Err(err) => {
                        warn!("Failed to get parent header for new slot: {:?}", err);
                        continue;
                    }
                }
            };

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
async fn wait_for_block_header<P>(
    block: B256,
    slot_time: OffsetDateTime,
    provider: P,
) -> eyre::Result<Header>
where
    P: HeaderProvider,
{
    let dead_line = slot_time + BLOCK_HEADER_DEAD_LINE_DELTA;
    while OffsetDateTime::now_utc() < dead_line {
        if let Some(header) = provider.header(&block)? {
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
