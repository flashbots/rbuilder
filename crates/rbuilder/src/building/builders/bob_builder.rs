use crate::building::block_orders::prioritized_order_store::OrderPriority;
use crate::building::evm_inspector::UsedStateTrace;
use crate::primitives::{OrderId, SimulatedOrder};
use crate::{
    building::builders::UnfinishedBlockBuildingSink,
    live_builder::{
        config::BobConfig, streaming::block_subscription_server::start_block_subscription_server,
    },
    primitives::{
        serialize::{RawBundle, TxEncoding},
        Bundle, Order,
    },
};
use ahash::HashMap;
use alloy_primitives::{B256, U256};
use alloy_rpc_types_eth::state::{AccountOverride, StateOverride};
use jsonrpsee::RpcModule;
use priority_queue::PriorityQueue;
use revm::db::BundleState;
use revm_primitives::map::B256HashMap;
use serde::Serialize;
use std::{
    fmt,
    net::Ipv4Addr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

use super::block_building_helper::{BiddableUnfinishedBlock, BlockBuildingHelperError};

#[derive(Clone, Debug)]
pub struct BobBuilderConfig {
    pub port: u16,
    pub ip: Ipv4Addr,
    pub stream_start_dur: Duration,
    /// time out used the send() of the channel used between the RPC and the processing task.
    pub channel_timeout: Duration,
    /// buffer size used on the channel used between the RPC and the processing task.
    pub channel_buffer_size: usize,
}

impl BobBuilderConfig {
    pub fn from_config(
        bob_config: &BobConfig,
        state_streaming_service_ip: Ipv4Addr,
        channel_timeout: Duration,
        channel_buffer_size: usize,
    ) -> BobBuilderConfig {
        BobBuilderConfig {
            port: bob_config.diff_server_port,
            stream_start_dur: Duration::from_millis(bob_config.stream_start_ms),
            ip: state_streaming_service_ip,
            channel_timeout,
            channel_buffer_size,
        }
    }
}

// There is a single bob instance for the entire builder process
// It server as a cache for our event handler loop to store partial blocks
// by uuid, and to store the rpc server responsible for streaming these blocks
// to bob searchers. The bob does not distinguish between partial blocks associated
// between different slots - it should be accessed through a handler which contains
// this association and logic.
#[derive(Clone, Debug)]
pub struct BobBuilder {
    config: BobBuilderConfig,
    inner: Arc<BobBuilderInner>,
}

struct BlockCacheEntry {
    block: BiddableUnfinishedBlock,
    sink: Arc<dyn UnfinishedBlockBuildingSink>,
}

struct BobBuilderInner {
    block_cache: Mutex<HashMap<Uuid, BlockCacheEntry>>,
    state_diff_server: broadcast::Sender<serde_json::Value>,
    bundle_queue: Mutex<PriorityQueue<OrderId, OrderPriority>>,
    bundle_orders: Mutex<HashMap<OrderId, SimulatedOrder>>,
}

impl fmt::Debug for BobBuilderInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BobBuilderInner").finish()
    }
}

impl BobBuilder {
    pub async fn new(config: BobBuilderConfig) -> eyre::Result<BobBuilder> {
        let server = start_block_subscription_server(config.ip, config.port)
            .await
            .expect("Failed to start block subscription server");
        let block_cache = HashMap::<Uuid, BlockCacheEntry>::default();
        Ok(Self {
            config,
            inner: Arc::new(BobBuilderInner {
                state_diff_server: server,
                block_cache: Mutex::new(block_cache),
                bundle_queue: Mutex::new(PriorityQueue::new()),
                bundle_orders: Mutex::new(HashMap::default()),
            }),
        })
    }

    pub fn server(&self) -> broadcast::Sender<serde_json::Value> {
        self.inner.state_diff_server.clone()
    }

    // BobBuilder should be accessed through a handler. This
    // handler will be associated with a particular slot, and
    // contains the relevants data fields for it. We spawn
    // a separate process that will wait for the cancellation token
    // associated with the slot, and then perform the necessary tear down.
    // Critically, this includes removing now stale partial blocks by uuid.
    pub fn new_handle(
        &self,
        sink: Arc<dyn UnfinishedBlockBuildingSink>,
        slot_timestamp: time::OffsetDateTime,
        cancel: CancellationToken,
    ) -> BobHandle {
        let handle = BobHandle {
            inner: Arc::new(Mutex::new(BobHandleInner {
                builder: self.clone(),
                canceled: false,
                highest_value: U256::from(0),
                slot_timestamp,
                sink,
                uuids: Vec::new(),
            })),
        };
        let handle_clone = handle.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {
                    handle_clone.inner.lock().unwrap().cancel();
                }
            }
        });
        handle
    }

    pub fn insert_block(
        &self,
        block: BiddableUnfinishedBlock,
        sink: Arc<dyn UnfinishedBlockBuildingSink>,
        uuid: Uuid,
    ) {
        let cache_entry = BlockCacheEntry { block, sink };
        self.inner
            .block_cache
            .lock()
            .unwrap()
            .insert(uuid, cache_entry);
    }
}

// Run bob builder is called once at startup of the builder.
// It attached an rpc method to receive bob orders from clients
// then enters and event handler loop that handles incoming orders
// until global cancellation. Blocks are looked up via uuid in cache,
// bob orders applied, and then forwards onto the final sink.
pub async fn run_bob_builder(
    bob_builder: &BobBuilder,
    cancel: CancellationToken,
) -> eyre::Result<(JoinHandle<()>, RpcModule<()>)> {
    let (order_sender, mut order_receiver) = mpsc::channel(bob_builder.config.channel_buffer_size);

    let timeout = bob_builder.config.channel_timeout;
    let mut module = RpcModule::new(());
    module.register_async_method("eth_sendBobBundle", move |params, _| {
        handle_eth_send_bob_bundle(order_sender.clone(), timeout, params)
    })?;

    let inner = bob_builder.inner.clone();
    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    break
                }
                Some((order, uuid)) = order_receiver.recv() => {
                    debug!("Received bob order for uuid: {:?} {:?}", order, uuid);
                    let (streamed_block, sink) = {
                        let cache = inner.block_cache.lock().unwrap();
                        if let Some(entry) = cache.get(&uuid) {
                            (entry.block.clone().into_building_helper(), entry.sink.clone())
                        } else {
                            continue;
                        }
                    };

                    let mut streamed_block = streamed_block;

                    match streamed_block.commit_order_with_trace(&order) {
                        Ok(Ok(execution_result_with_trace)) => {
                            let execution_result =execution_result_with_trace.execution_result;
                            info!(
                                ?uuid,
                                order_id=?order.id(),
                                profit=?execution_result.inplace_sim.coinbase_profit,
                                gas_used=execution_result.gas_used,
                                used_state_trace=?execution_result_with_trace.used_state_trace,
                                "SUCCESSFULLY COMMITTED ORDER"
                            );

                            // Insert the order into our bundle cache with appropriate priority
                            let order_id = order.id();
                            let priority = OrderPriority {
                                order_id,
                                priority: execution_result.inplace_sim.coinbase_profit.to::<u128>(),
                            };

                            // Create SimulatedOrder from the successful commit
                            let sim_order = SimulatedOrder {
                                order: execution_result.order.clone(),
                                sim_value: execution_result.inplace_sim.clone(),
                                used_state_trace: Some(execution_result_with_trace.used_state_trace.clone()),
                            };

                            info!(?sim_order.used_state_trace, "Used state trace");

                            // Insert into both cache structures under mutex protection
                            {
                                let mut bundle_queue = inner.bundle_queue.lock().unwrap();
                                let mut bundle_orders = inner.bundle_orders.lock().unwrap();

                                bundle_queue.push(order_id, priority);
                                bundle_orders.insert(order_id, sim_order);
                            }
                            match BiddableUnfinishedBlock::new(streamed_block){
                                Ok(biddable_streamed_block) => {
                                    sink.new_block(biddable_streamed_block);
                                    info!(?uuid, order_id=?order.id(), "STEP 7: ORDER FULLY PROCESSED AND CACHED");
                                },
                                Err(err) => {
                                    warn!(?uuid, order_id=?order.id(),?err,"Unable to close Bob block (Bob order broke something?)");
                                },
                            }
                        }
                        Ok(Err(e)) => {
                            info!("Reverted or failed bob order: {:?}", e);
                        }
                        Err(e) => {
                            info!("Error commiting bob order: {:?}", e);
                        }
                    }
                }
            }
        }
    });

    Ok((handle, module))
}

/// Parses parameters as [raw_bundle,uuid] and sends the result to result_channel.
async fn handle_eth_send_bob_bundle(
    result_channel: mpsc::Sender<(Order, Uuid)>,
    timeout: Duration,
    params: jsonrpsee::types::Params<'static>,
) {
    let start = Instant::now();
    let mut seq = params.sequence();
    let raw_bundle: Result<RawBundle, _> = seq.next();
    let uuid: Result<Uuid, _> = seq.next();
    let (raw_bundle, uuid) = if let (Ok(raw_bundle), Ok(uuid)) = (raw_bundle, uuid) {
        (raw_bundle, uuid)
    } else {
        warn!("Failed to parse send_bob_bundle");
        return;
    };

    let bundle: Bundle = match raw_bundle.decode_new_bundle(TxEncoding::WithBlobData) {
        Ok(bundle) => bundle,
        Err(err) => {
            warn!(?err, "Failed to parse bundle");
            // @Metric
            return;
        }
    };

    let order = Order::Bundle(bundle);
    let parse_duration = start.elapsed();
    let target_block = order.target_block().unwrap_or_default();
    trace!(order = ?order.id(), parse_duration_mus = parse_duration.as_micros(), target_block, "Received bundle");
    match result_channel.send_timeout((order, uuid), timeout).await {
        Ok(()) => {}
        Err(mpsc::error::SendTimeoutError::Timeout(_)) => {}
        Err(mpsc::error::SendTimeoutError::Closed(_)) => {}
    };
}

// BobHandle associate a particular slot to the BobBuilder,
// and store relevant information about the slot, the uuid of partial blocks
// generated for that slot, highest value observed, and final sealer / bidding sink
// The BobBuilder is not accessed directly, it should be only be accessed through the handle.
//
// It implemented the UnfinishedBlockBuilderSink interface so it can act be directly
// used as a sink for other building algorithms.
#[derive(Clone, Debug)]
pub struct BobHandle {
    inner: Arc<Mutex<BobHandleInner>>,
}

impl UnfinishedBlockBuildingSink for BobHandle {
    fn new_block(&self, block: BiddableUnfinishedBlock) {
        // Stream the block to searchers
        self.inner.lock().unwrap().stream_block(block.clone());

        // Try to fill with cached bundle orders
        match self.inner.lock().unwrap().fill_bob_orders(block) {
            Ok(biddable_block) =>
            // Pass filled block to sink
            {
                self.inner.lock().unwrap().sink.new_block(biddable_block)
            }
            Err(err) => {
                warn!(
                    ?err,
                    "Unable to close Bob upgraded block (Bob order broke something?)"
                );
            }
        }
    }

    fn can_use_suggested_fee_recipient_as_coinbase(&self) -> bool {
        return self
            .inner
            .lock()
            .unwrap()
            .can_use_suggested_fee_recipient_as_coinbase();
    }
}

#[derive(Clone, Debug)]
struct BobHandleInner {
    builder: BobBuilder,
    canceled: bool,
    highest_value: U256,
    sink: Arc<dyn UnfinishedBlockBuildingSink>,
    slot_timestamp: time::OffsetDateTime,
    uuids: Vec<Uuid>,
}

impl BobHandleInner {
    fn stream_block(&mut self, block: BiddableUnfinishedBlock) {
        // If we've processed a cancellation for this slot, bail.
        if self.canceled {
            return;
        }

        // We only stream new partial blocks to searchers in a default 2 second window
        // before the slot end. We don't need to store partial blocks not streamed so bail.
        if !self.in_stream_window() {
            return;
        }
        // Only stream new partial blocks whose non-bob value is an increase.
        if !self.check_and_store_block_value(&block) {
            return;
        }
        trace!("Streaming bob partial block");

        let block_uuid = Uuid::new_v4();
        self.uuids.push(block_uuid);

        let building_context = block.building_context();
        let bundle_state = block.get_bundle_state();

        // Create state update object containing block info and state differences
        let block_state_update = BlockStateUpdate {
            block_number: building_context.evm_env.block_env.number,
            block_timestamp: building_context.evm_env.block_env.timestamp,
            block_uuid,
            gas_remaining: block.gas_remaining(),
            state_overrides: bundle_state_to_state_overrides(bundle_state),
        };

        // Insert the block in the builder cache before streaming to searchers
        // in order to avoid a potential race condition where a searcher could respond
        // to the streamed value before it's been inserted and made known to the BobBuilder
        //
        // The actual rpc message is constructed above to avoid creating an uncessary clone
        // due to ownership rules.
        self.builder
            .insert_block(block, self.sink.clone(), block_uuid);

        // Get block context and state
        match serde_json::to_value(&block_state_update) {
            Ok(json_data) => {
                if let Err(_e) = self.builder.inner.state_diff_server.send(json_data) {
                    warn!("Failed to send block data");
                } else {
                    info!(
                        "Sent BlockStateUpdate: uuid={}",
                        block_state_update.block_uuid
                    );
                }
            }
            Err(e) => error!("Failed to serialize block state diff update: {:?}", e),
        }
    }

    fn can_use_suggested_fee_recipient_as_coinbase(&self) -> bool {
        self.sink.can_use_suggested_fee_recipient_as_coinbase()
    }

    // Checks if we're in the streaming window
    fn in_stream_window(&self) -> bool {
        let now = time::OffsetDateTime::now_utc();
        let delta = self.slot_timestamp - now;

        delta < self.builder.config.stream_start_dur
    }

    fn check_and_store_block_value(&mut self, block: &BiddableUnfinishedBlock) -> bool {
        let true_block_value = block.true_block_value();
        if true_block_value > self.highest_value {
            self.highest_value = true_block_value;
            true
        } else {
            false
        }
    }

    // Performs teardown for the handle, triggered above when the slot cancellation
    // token is triggered. Removes uuids we've seen from the builder cache.
    //
    // Note that we store that the cancellation has occured - otherwise there could
    // be stale blocks inserted after cancellation due to race conditions in when upstream
    // processed receive / handle cancellation. E.G. our teardown occurs before an upstream builder
    // has handle the cancellation.
    pub fn cancel(&mut self) {
        // Clear block cache
        let mut cache = self.builder.inner.block_cache.lock().unwrap();
        self.uuids.iter().for_each(|uuid| {
            cache.remove(uuid);
        });

        // Clear bundle queue and orders for next slot
        let mut bundle_queue = self.builder.inner.bundle_queue.lock().unwrap();
        let mut bundle_orders = self.builder.inner.bundle_orders.lock().unwrap();
        bundle_queue.clear();
        bundle_orders.clear();
        info!("cancelled");

        self.canceled = true;
    }

    fn validate_storage_reads(
        bundle_state: &BundleState,
        used_state_trace: &UsedStateTrace,
    ) -> bool {
        // Iterate through all read slot values from the simulated order
        for (read_slot_key, value) in &used_state_trace.read_slot_values {
            // Check if the address exists in bundle_state
            if let Some(bundle_account) = bundle_state.state.get(&read_slot_key.address) {
                // If address exists, check if the specific storage slot read still has the same value
                if let Some(storage_slot) = bundle_account.storage.get(&read_slot_key.key.into()) {
                    let original_value: U256 = (*value).into();
                    if storage_slot.present_value != original_value {
                        info!(
                            address = ?read_slot_key.address,
                            slot = ?read_slot_key.key,
                            read_value = ?original_value,
                            current_value = ?storage_slot.present_value,
                            "Storage value changed"
                        );
                        return false;
                    }
                }
                // If storage slot doesn't exist in bundle_state, it means it hasn't changed
                // so we can continue checking other slots
            }
            // If address doesn't exist in bundle_state, it means no changes were made
            // so we can continue checking other slots
        }

        // All read slots either match or weren't modified
        true
    }

    fn fill_bob_orders(
        &mut self,
        block: BiddableUnfinishedBlock,
    ) -> Result<BiddableUnfinishedBlock, BlockBuildingHelperError> {
        let bundle_queue = self.builder.inner.bundle_queue.lock().unwrap();
        let bundle_orders = self.builder.inner.bundle_orders.lock().unwrap();

        let mut block = block.into_building_helper();
        // Try each order in priority order while we have enough gas
        for (order_id, _) in bundle_queue.iter() {
            if let Some(order) = bundle_orders.get(order_id) {
                // Add validation check before attempting to commit
                if let Some(ref used_state_trace) = order.used_state_trace {
                    if !Self::validate_storage_reads(block.get_bundle_state(), used_state_trace) {
                        continue;
                    }
                }

                match block.commit_sim_order(order) {
                    Ok(Ok(_)) => {
                        info!(
                            order_id = ?order.order.id(),
                            "No storage changes - committed order!"
                        );
                    }
                    Ok(Err(_err)) => {}
                    Err(err) => {
                        info!(
                            ?err,
                            order_id = ?order.order.id(),
                            "Critical error committing cached order"
                        );
                    }
                }
            }
        }
        BiddableUnfinishedBlock::new(block)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BlockStateUpdate {
    block_number: U256,
    block_timestamp: U256,
    block_uuid: Uuid,
    gas_remaining: u64,
    state_overrides: StateOverride,
}

// BundleState is the object used to track the accumulated state changes from
// sequential transaction.
//
// We convert this into a StateOverride object, which is a more standard format
// used in other contexts to specifies state overrides, such as in eth_call and
// other simulation methods.
fn bundle_state_to_state_overrides(bundle_state: &BundleState) -> StateOverride {
    let account_overrides: StateOverride = bundle_state
        .state
        .iter()
        .filter_map(|(address, bundle_account)| {
            let info = bundle_account.info.as_ref()?;
            if info.is_empty_code_hash() {
                return None;
            }
            let code = bundle_state
                .contracts
                .get(&info.code_hash)
                .map(|code| code.bytes().clone());

            let storage_diff: B256HashMap<B256> = bundle_account
                .storage
                .iter()
                .map(|(slot, storage_slot)| {
                    (B256::from(*slot), B256::from(storage_slot.present_value))
                })
                .collect();
            let account_override = AccountOverride {
                balance: Some(info.balance),
                nonce: Some(info.nonce),
                code,
                state_diff: if storage_diff.is_empty() {
                    None
                } else {
                    Some(storage_diff)
                },
                state: None,
                move_precompile_to: None,
            };

            Some((*address, account_override))
        })
        .collect();

    account_overrides
}
