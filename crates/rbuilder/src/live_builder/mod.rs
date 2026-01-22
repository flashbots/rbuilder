pub mod base_config;
pub mod block_list_provider;
pub mod block_output;
pub mod building;
pub mod cli;
pub mod config;
pub mod order_flow_tracing;
pub mod order_input;
pub mod payload_events;
pub mod process_killer;
pub mod simulation;
pub mod wallet_balance_watcher;
pub mod watchdog;

use crate::{
    building::{builders::BlockBuildingAlgorithm, BlockBuildingContext},
    live_builder::{
        order_flow_tracing::order_flow_tracer_manager::OrderFlowTracerManager,
        order_input::{start_orderpool_jobs, OrderInputConfig},
        process_killer::{ProcessKiller, CRITICAL_SHUTDOWN_TIMEOUT, GRACEFUL_SHUTDOWN_TIMEOUT},
        simulation::OrderSimulationPool,
    },
    provider::StateProviderFactory,
    telemetry::{inc_active_slots, mark_building_started},
    utils::{
        error_storage::spawn_error_storage_writer, format_offset_datetime_rfc3339,
        mevblocker::get_mevblocker_price, provider_head_state::ProviderHeadState, Signer,
    },
};
use alloy_consensus::Header;
use alloy_primitives::{BlockHash, B256};
use block_list_provider::BlockListProvider;
use block_output::unfinished_block_processing::UnfinishedBuiltBlocksInputFactory;
use building::BlockBuildingPool;
use eyre::Context;
use futures::{stream::FuturesUnordered, StreamExt};
use jsonrpsee::RpcModule;
use order_input::ReplaceableOrderPoolCommand;
use payload_events::{InternalPayloadId, MevBoostSlotDataGenerator};
use rbuilder_primitives::{MempoolTx, Order, TransactionSignedEcRecoveredWithBlobs};
use reth::transaction_pool::{
    BlobStore, EthPooledTransaction, Pool, TransactionListenerKind, TransactionOrdering,
    TransactionPool, TransactionValidator,
};
use reth_chainspec::ChainSpec;
use reth_primitives::{Recovered, TransactionSigned};
use std::{
    cmp::min,
    fmt::Debug,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use time::OffsetDateTime;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::*;

#[derive(Debug, Clone)]
pub struct TimingsConfig {
    /// Time the proposer have to propose a block from the beginning of the
    /// slot (https://www.paradigm.xyz/2023/04/mev-boost-ethereum-consensus Slot anatomy)
    pub slot_proposal_duration: Duration,
    /// Delta from slot time to get_header dead line. If we can't get the block header
    /// before slot_time + BLOCK_HEADER_DEAD_LINE_DELTA we cancel the slot.
    /// Careful: It's signed and usually negative since we need de header BEFORE the slot time.
    pub block_header_deadline_delta: time::Duration,
    /// Polling period while trying to get a block header
    pub get_block_header_period: time::Duration,
}

impl TimingsConfig {
    /// Classic rbuilder
    pub fn ethereum() -> Self {
        Self {
            slot_proposal_duration: Duration::from_secs(4),
            block_header_deadline_delta: time::Duration::milliseconds(-2500),
            get_block_header_period: time::Duration::milliseconds(250),
        }
    }

    /// Configuration for OP-based chains with fast block times
    pub fn optimism() -> Self {
        Self {
            slot_proposal_duration: Duration::from_secs(0),
            block_header_deadline_delta: time::Duration::milliseconds(-25),
            get_block_header_period: time::Duration::milliseconds(25),
        }
    }
}

/// Max headers sent to the cleaning task before the main loop blocks.
/// Cleaning task is super fast so it should never lag behind block building, even 1 should be enough, 10 is super safe.
const CLEAN_TASKS_CHANNEL_SIZE: usize = 10;

/// Main builder struct.
/// Connects to the CL, get the new slots and builds blocks for each slot.
/// # Usage
/// Create and run()
#[derive(Debug)]
pub struct LiveBuilder<P>
where
    P: StateProviderFactory,
{
    pub watchdog_timeout: Option<Duration>,
    pub error_storage_path: Option<PathBuf>,
    pub simulation_threads: usize,
    pub order_input_config: OrderInputConfig,
    pub blocks_source: MevBoostSlotDataGenerator,
    pub run_sparse_trie_prefetcher: bool,

    pub chain_chain_spec: Arc<ChainSpec>,
    pub provider: P,

    pub coinbase_signer: Signer,
    pub extra_data: Vec<u8>,
    pub blocklist_provider: Arc<dyn BlockListProvider>,

    /// This cancellation token is used to cancel block building in a nice way.
    /// Every task getting this token should try to cancel asap if possible without any data corruption.
    pub global_cancellation: CancellationToken,
    pub process_killer: ProcessKiller,
    /// If after some time rbuilder does not achieve graceful shutdown with global_cancellation it will signal this token.
    /// Task getting this token should abort any waiting/processing of new data assuming upstream data source is hanged and try to
    /// perform cleanup asap.
    pub global_abort: CancellationToken,
    /// We should try to wait for these tasks to finish before shutting down to avoid data corruption.
    pub critical_tasks_join_handles: Vec<JoinHandle<()>>,

    pub unfinished_built_blocks_input_factory: UnfinishedBuiltBlocksInputFactory<P>,
    pub builders: Vec<Arc<dyn BlockBuildingAlgorithm<P>>>,
    pub extra_rpc: RpcModule<()>,

    /// Notify rbuilder of new [`ReplaceableOrderPoolCommand`] flow via this channel.
    pub orderpool_sender: mpsc::Sender<ReplaceableOrderPoolCommand>,
    pub orderpool_receiver: mpsc::Receiver<ReplaceableOrderPoolCommand>,

    pub evm_caching_enable: bool,
    pub faster_finalize: bool,
    pub simulation_use_random_coinbase: bool,

    pub order_flow_tracer_manager: Box<dyn OrderFlowTracerManager>,
}

impl<P> LiveBuilder<P>
where
    P: StateProviderFactory + Clone + 'static,
{
    pub fn with_extra_rpc(self, extra_rpc: RpcModule<()>) -> Self {
        Self { extra_rpc, ..self }
    }

    pub fn with_builders(self, builders: Vec<Arc<dyn BlockBuildingAlgorithm<P>>>) -> Self {
        Self { builders, ..self }
    }

    pub fn add_critical_task(&mut self, handle: JoinHandle<()>) {
        self.critical_tasks_join_handles.push(handle);
    }

    pub async fn run(
        mut self,
        ready_to_build: Arc<AtomicBool>, // If Some, we should send a message for every slot we start building.
        start_slot_watchdog_sender: Option<flume::Sender<()>>,
    ) -> eyre::Result<()> {
        let global_cancellation = self.global_cancellation.clone();
        let global_abort = self.global_abort.clone();
        let critical_tasks_join_handles = std::mem::take(&mut self.critical_tasks_join_handles);

        let mut inner_jobs_handles = Vec::new();
        let res = self
            .run_no_cleanup(
                ready_to_build,
                &mut inner_jobs_handles,
                start_slot_watchdog_sender,
            )
            .await;
        shutdown_builder(
            global_cancellation,
            global_abort,
            inner_jobs_handles,
            critical_tasks_join_handles,
        )
        .await;
        res
    }

    /// Run the builder without cleaning up after itself.
    pub async fn run_no_cleanup(
        self,
        ready_to_build: Arc<AtomicBool>,
        inner_jobs_handles: &mut Vec<JoinHandle<()>>,
        // If Some, we should send a message for every slot we start building.
        start_slot_watchdog_sender: Option<flume::Sender<()>>,
    ) -> eyre::Result<()> {
        info!(
            "Builder initial block list size: {}",
            self.blocklist_provider.get_blocklist()?.len(),
        );
        info!(
            "Builder coinbase address: {:?}",
            self.coinbase_signer.address
        );
        let timings = self.timings();

        if let Some(error_storage_path) = self.error_storage_path {
            spawn_error_storage_writer(error_storage_path, self.global_cancellation.clone())
                .await
                .with_context(|| "Error spawning error storage writer")?;
        }

        let mut payload_events_channel = self.blocks_source.recv_slot_channel();

        let (header_sender, header_receiver) = mpsc::channel(CLEAN_TASKS_CHANNEL_SIZE);

        let orderpool_subscriber = {
            let (handle, sub) = start_orderpool_jobs(
                self.order_input_config,
                self.provider.clone(),
                self.extra_rpc,
                self.global_cancellation.clone(),
                self.orderpool_sender,
                self.orderpool_receiver,
                header_receiver,
            )
            .await?;
            inner_jobs_handles.push(handle);
            sub
        };

        let order_simulation_pool = OrderSimulationPool::new(
            self.provider.clone(),
            self.simulation_threads,
            self.simulation_use_random_coinbase,
            self.global_cancellation.clone(),
        );

        let mut builder_pool = BlockBuildingPool::new(
            self.provider.clone(),
            self.builders,
            self.unfinished_built_blocks_input_factory,
            orderpool_subscriber,
            order_simulation_pool,
            self.run_sparse_trie_prefetcher,
            self.order_flow_tracer_manager,
        );

        ready_to_build.store(true, Ordering::Relaxed);
        while let Some(payload) = payload_events_channel.recv().await {
            let blocklist = self.blocklist_provider.get_blocklist()?;
            if blocklist.contains(&payload.fee_recipient()) {
                warn!(
                    slot = payload.slot(),
                    fee_recipient = ?payload.fee_recipient(),
                    payload_id = payload.payload_id,
                    "Fee recipient is in blocklist"
                );
                continue;
            }
            let current_time = OffsetDateTime::now_utc();
            // see if we can get parent header in a reasonable time
            let time_to_slot = payload.timestamp() - current_time;
            debug!(
                slot = payload.slot(),
                block = payload.block(),
                payload_id = payload.payload_id,
                payload_timestamp = format_offset_datetime_rfc3339(&payload.timestamp()),
                time_to_slot_s = time_to_slot.as_seconds_f64(),
                parent_hash = ?payload.parent_block_hash(),
                provider_head_state = ?ProviderHeadState::new(&self.provider),
                "Received payload, time till slot timestamp",
            );

            let time_until_slot_end = time_to_slot + timings.slot_proposal_duration;
            if time_until_slot_end.is_negative() {
                warn!(
                    slot = payload.slot(),
                    block = payload.block(),
                    payload_id = payload.payload_id,
                    parent_hash = ?payload.parent_block_hash(),
                    "Slot already ended, skipping block building"
                );
                continue;
            };

            let parent_header = {
                // @Nicer
                let parent_block = payload.parent_block_hash();
                let timestamp = payload.timestamp();
                let block_number = payload.block();
                match wait_for_block_header(
                    block_number,
                    parent_block,
                    payload.payload_id,
                    timestamp,
                    &self.provider,
                    &timings,
                )
                .await
                {
                    Ok(header) => header,
                    Err(err) => {
                        let last_blocks = get_last_blocks(&self.provider).await;
                        let blocks_count_behind =
                            payload.block() - last_blocks.first().unwrap_or(&(0, None)).0;
                        warn!(payload_id = payload.payload_id, parent_hash = ?payload.parent_block_hash(), ?last_blocks, blocks_count_behind, ?err, "Failed to get parent header for new slot");
                        continue;
                    }
                }
            };

            debug!(
                slot = payload.slot(),
                block = payload.block(),
                payload_id = payload.payload_id,
                parent_hash = %payload.parent_block_hash(),
                "Got header for slot"
            );

            // notify the order pool that there is a new header
            if let Err(err) = header_sender.send(parent_header.clone()).await {
                warn!(?err, "Failed to send header to builder pool");
            }

            inc_active_slots();

            // Retrieve MEV block price.
            let mev_blocker_price = get_mevblocker_price(
                self.provider
                    .history_by_block_hash(payload.parent_block_hash())?,
            )?;

            let root_hasher =
                Arc::from(self.provider.root_hasher(payload.parent_block_num_hash())?);

            if let Some(block_ctx) = BlockBuildingContext::from_attributes(
                payload.payload_attributes_event.clone(),
                &parent_header,
                self.coinbase_signer,
                self.chain_chain_spec.clone(),
                blocklist.clone(),
                Some(payload.suggested_gas_limit),
                self.extra_data.clone(),
                None,
                root_hasher,
                payload.payload_id,
                self.evm_caching_enable,
                self.faster_finalize,
                mev_blocker_price,
                payload
                    .relay_registrations
                    .iter()
                    .filter_map(|(_, r)| r.adjustment_fee_payer)
                    .collect(),
            ) {
                mark_building_started(block_ctx.timestamp());
                builder_pool.start_block_building(
                    payload,
                    block_ctx,
                    self.global_cancellation.clone(),
                    time_until_slot_end.try_into().unwrap_or_default(),
                );
                if let Some(watchdog_sender) = start_slot_watchdog_sender.as_ref() {
                    watchdog_sender.try_send(()).unwrap_or_default();
                };
            }
        }
        Ok(())
    }

    /// Connect the builder to a reth [`TransactionPool`].
    ///
    /// This will
    /// 1. Add pending and queued transactions to the [`OrderPool`]
    /// 2. Subscribe to the pool directly, so the builder is not reliant on
    ///    IPC to be notified of new transactions.
    pub async fn connect_to_transaction_pool<V, T, S>(
        &self,
        pool: Pool<V, T, S>,
    ) -> Result<(), eyre::Error>
    where
        V: TransactionValidator<Transaction = EthPooledTransaction> + 'static,
        T: TransactionOrdering<Transaction = <V as TransactionValidator>::Transaction>,
        S: BlobStore,
    {
        // Initialize the orderpool with every item in the reth pool.
        for tx in pool
            .all_transactions()
            .pending_recovered()
            .chain(pool.all_transactions().queued_recovered())
        {
            try_send_to_orderpool(tx, self.orderpool_sender.clone(), pool.clone()).await;
        }

        // Subscribe to new transactions in-process.
        let mut recv = pool.new_transactions_listener_for(TransactionListenerKind::All);
        let orderpool_sender = self.orderpool_sender.clone();
        tokio::spawn(async move {
            while let Some(e) = recv.recv().await {
                let tx = e.transaction.transaction.transaction().clone();
                try_send_to_orderpool(tx, orderpool_sender.clone(), pool.clone()).await;
            }
        });

        Ok(())
    }

    // Currently we only need two timings config, depending on whether rbuilder is being
    // used in the optimism context. If further customisation is required in the future
    // this should be improved on.
    fn timings(&self) -> TimingsConfig {
        if cfg!(feature = "optimism") {
            TimingsConfig::optimism()
        } else {
            TimingsConfig::ethereum()
        }
    }
}

/// We bring 10 blocks since that's more than enough to see the evolution of the chain.
/// We could probably be ok even with only the last block.
const LAST_BLOCKS_COUNT: usize = 10;
/// Get the last blocks in decreasing order for debugging purposes.
/// Returns a vector of tuples (block_number, header_hash) for available blocks.
/// On error returns empty.
pub async fn get_last_blocks<P>(provider: &P) -> Vec<(u64, Option<BlockHash>)>
where
    P: StateProviderFactory,
{
    let mut result = Vec::new();

    // Get the last block number
    let last_block_num = match provider.last_block_number() {
        Ok(num) => num,
        Err(err) => {
            warn!(?err, "Failed to get last block number");
            return result;
        }
    };

    // Get the last headers going backwards from the last block
    for i in 0..LAST_BLOCKS_COUNT {
        let block_num = last_block_num.saturating_sub(i as u64);
        let header_hash = provider
            .header_by_number(block_num)
            .ok()
            .flatten()
            .map(|h| h.hash_slow());

        result.push((block_num, header_hash));
    }

    result
}

/// May fail if we wait too much (see [BLOCK_HEADER_DEAD_LINE_DELTA])
async fn wait_for_block_header<P>(
    block: u64,
    parent_hash: B256,
    payload_id: InternalPayloadId,
    slot_time: OffsetDateTime,
    provider: &P,
    timings: &TimingsConfig,
) -> eyre::Result<Header>
where
    P: StateProviderFactory,
{
    let deadline = slot_time + timings.block_header_deadline_delta;
    let mut sleep_duration: Option<Duration> = None;
    loop {
        if let Some(sleep_duration) = sleep_duration.take() {
            tokio::time::sleep(sleep_duration).await;
        }

        if let Some(header) = provider.header(&parent_hash)? {
            return Ok(header);
        } else {
            let current_parent_hash = provider
                .header_by_number(block.checked_sub(1).unwrap_or(1))?
                .map(|h| h.hash_slow());
            info!(
                block,
                ?parent_hash,
                ?current_parent_hash,
                payload_id,
                "Payload parent header not found, trying again"
            );

            let time_to_sleep = min(
                deadline - OffsetDateTime::now_utc(),
                timings.get_block_header_period,
            );
            if time_to_sleep.is_negative() {
                sleep_duration = None;
            } else {
                sleep_duration = Some(time_to_sleep.try_into().unwrap());
            }
        }

        if OffsetDateTime::now_utc() > deadline {
            break;
        }
    }
    Err(eyre::eyre!("Block header not found"))
}

/// Attempts to forward a [`Recovered<TransactionSigned>`] to an orderpool.
///
/// Helper for [`LiveBuilder::connect_to_transaction_pool`].
///
/// Errors are handled internally with a log.
async fn try_send_to_orderpool<V, T, S>(
    tx: Recovered<TransactionSigned>,
    orderpool_sender: mpsc::Sender<ReplaceableOrderPoolCommand>,
    pool: Pool<V, T, S>,
) where
    V: TransactionValidator<Transaction = EthPooledTransaction> + 'static,
    T: TransactionOrdering<Transaction = <V as TransactionValidator>::Transaction>,
    S: BlobStore,
{
    match TransactionSignedEcRecoveredWithBlobs::try_from_tx_without_blobs_and_pool(tx, pool) {
        Ok(tx) => {
            let order = Order::Tx(MempoolTx::new(tx));
            let command = ReplaceableOrderPoolCommand::Order(order);
            if let Err(e) = orderpool_sender.send(command).await {
                error!("Error sending order to orderpool: {:#}", e);
            }
        }
        Err(e) => {
            error!("Error creating order from transaction: {:#}", e);
        }
    }
}

/// Gracefully shuts down the builder, waiting for tasks to complete with timeouts.
///
/// First waits for all tasks (inner jobs + critical tasks) with a graceful timeout.
/// If the timeout is reached, signals the abort token and waits only for critical tasks
/// with a second timeout.
async fn shutdown_builder(
    global_cancellation: CancellationToken,
    global_abort: CancellationToken,
    inner_jobs_handles: Vec<JoinHandle<()>>,
    critical_tasks_join_handles: Vec<JoinHandle<()>>,
) {
    info!("Builder shutting down");
    global_cancellation.cancel();

    // Collect handles into FuturesUnordered for concurrent waiting
    let mut inner_jobs: FuturesUnordered<_> = inner_jobs_handles.into_iter().collect();
    let mut critical_tasks: FuturesUnordered<_> = critical_tasks_join_handles.into_iter().collect();

    // First phase: wait for all handles with graceful timeout
    let graceful_deadline = tokio::time::Instant::now() + GRACEFUL_SHUTDOWN_TIMEOUT;

    loop {
        if inner_jobs.is_empty() && critical_tasks.is_empty() {
            break;
        }

        tokio::select! {
            biased;
            Some(result) = inner_jobs.next(), if !inner_jobs.is_empty() => {
                if let Err(err) = result {
                    warn!(?err, "Job handle await error");
                }
            }
            Some(result) = critical_tasks.next(), if !critical_tasks.is_empty() => {
                if let Err(err) = result {
                    warn!(?err, "Critical task handle await error");
                }
            }
            _ = tokio::time::sleep_until(graceful_deadline) => {
                warn!("Graceful shutdown timeout reached, signaling abort");
                global_abort.cancel();
                break;
            }
        }
    }

    // Second phase: if there are remaining critical tasks, wait with second timeout
    if !critical_tasks.is_empty() {
        let critical_deadline = tokio::time::Instant::now() + CRITICAL_SHUTDOWN_TIMEOUT;
        loop {
            tokio::select! {
                biased;
                result = critical_tasks.next() => {
                    match result {
                        Some(Err(err)) => warn!(?err, "Critical task handle await error"),
                        Some(Ok(())) => {}
                        None => break,
                    }
                }
                _ = tokio::time::sleep_until(critical_deadline) => {
                    warn!("Critical tasks shutdown timeout reached");
                    break;
                }
            }
        }
    } else {
        info!(
            non_critical_task_remaining = inner_jobs.len(),
            "No critical tasks remaining, shutting down",
        );
    }
}
