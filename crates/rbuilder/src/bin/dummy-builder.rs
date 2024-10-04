//! This simple app shows how to run a custom block builder.
//! It uses no bidding strategy, it just bids all available profit.
//! It does not sends blocks to any relay, it just logs the generated blocks.
//! The algorithm is really dummy, it just adds some txs it receives and generates a single block.
//! This is NOT intended to be run in production so it has no nice configuration, poor error checking and some hardcoded values.
use std::{path::PathBuf, sync::Arc, thread::sleep, time::Duration};

use rbuilder::{
    beacon_api_client::Client,
    building::{
        builders::{
            block_building_helper::{BlockBuildingHelper, BlockBuildingHelperFromDB},
            BlockBuildingAlgorithm, BlockBuildingAlgorithmInput, OrderConsumer,
            UnfinishedBlockBuildingSink, UnfinishedBlockBuildingSinkFactory,
        },
        BlockBuildingContext, SimulatedOrderStore,
    },
    live_builder::{
        base_config::{
            DEFAULT_EL_NODE_IPC_PATH, DEFAULT_INCOMING_BUNDLES_PORT, DEFAULT_IP,
            DEFAULT_RETH_DB_PATH,
        },
        config::create_provider_factory,
        order_input::{
            OrderInputConfig, DEFAULT_INPUT_CHANNEL_BUFFER_SIZE, DEFAULT_RESULTS_CHANNEL_TIMEOUT,
            DEFAULT_SERVE_MAX_CONNECTIONS,
        },
        payload_events::{MevBoostSlotData, MevBoostSlotDataGenerator},
        simulation::SimulatedOrderCommand,
        LiveBuilder,
    },
    primitives::{
        mev_boost::{MevBoostRelay, RelayConfig},
        SimulatedOrder,
    },
    roothash::RootHashConfig,
    utils::Signer,
};
use reth::{providers::ProviderFactory, tasks::pool::BlockingTaskPool};
use reth_chainspec::MAINNET;
use reth_db::{database::Database, DatabaseEnv};
use tokio::{signal::ctrl_c, sync::broadcast};
use tokio_util::sync::CancellationToken;
use tracing::{info, level_filters::LevelFilter};

// state diff stream imports
use futures::StreamExt;
use jsonrpsee::core::server::SubscriptionMessage;
use jsonrpsee::server::{RpcModule, Server};
use jsonrpsee::PendingSubscriptionSink;
use tokio_stream::wrappers::BroadcastStream;
use serde_json::json;
use tracing::warn;
use alloy_primitives::{B256};
use alloy_rpc_types_eth::state::{StateOverride, AccountOverride};
use std::collections::HashMap;
use uuid::Uuid;

const RETH_DB_PATH: &str = DEFAULT_RETH_DB_PATH;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let env =
        tracing_subscriber::EnvFilter::from_default_env().add_directive(LevelFilter::INFO.into());
    let writer = tracing_subscriber::fmt()
        .with_env_filter(env)
        .with_test_writer();
    writer.init();
    let chain_spec = MAINNET.clone();
    let cancel = CancellationToken::new();

    let relay_config = RelayConfig::default().
        with_url("https://0xac6e77dfe25ecd6110b8e780608cce0dab71fdd5ebea22a16c0205200f2f8e2e3ad3b71d3499c54ad14d6c21b41a37ae@boost-relay.flashbots.net").
        with_name("flashbots");

    let relay = MevBoostRelay::from_config(&relay_config)?;

    let payload_event = MevBoostSlotDataGenerator::new(
        vec![Client::default()],
        vec![relay],
        Default::default(),
        cancel.clone(),
    );

    // we create a sink factory which will contain our jsonrpc server
    let sink_factory = TraceBlockSinkFactory::new().await?;

    let builder = LiveBuilder::<Arc<DatabaseEnv>, MevBoostSlotDataGenerator> {
        watchdog_timeout: Duration::from_secs(10000),
        error_storage_path: None,
        simulation_threads: 1,
        blocks_source: payload_event,
        order_input_config: OrderInputConfig::new(
            false,
            true,
            DEFAULT_EL_NODE_IPC_PATH.parse().unwrap(),
            DEFAULT_INCOMING_BUNDLES_PORT,
            *DEFAULT_IP,
            DEFAULT_SERVE_MAX_CONNECTIONS,
            DEFAULT_RESULTS_CHANNEL_TIMEOUT,
            DEFAULT_INPUT_CHANNEL_BUFFER_SIZE,
        ),
        chain_chain_spec: chain_spec.clone(),
        provider_factory: create_provider_factory(
            Some(&RETH_DB_PATH.parse::<PathBuf>().unwrap()),
            None,
            None,
            chain_spec.clone(),
        )?,
        coinbase_signer: Signer::random(),
        extra_data: Vec::new(),
        blocklist: Default::default(),
        global_cancellation: cancel.clone(),
        extra_rpc: RpcModule::new(()),
        sink_factory: Box::new(sink_factory), // pointer to sink factory
        builders: vec![Arc::new(DummyBuildingAlgorithm::new(10))],
    };

    let ctrlc = tokio::spawn(async move {
        ctrl_c().await.unwrap_or_default();
        cancel.cancel()
    });

    builder.run().await?;
    ctrlc.await.unwrap_or_default();
    Ok(())
}

/////////////////////////
/// BLOCK SINK (we have created our jsonrpc server here)
/////////////////////////
#[derive(Debug)]
struct TraceBlockSinkFactory {
    tx: broadcast::Sender<String>,
}

impl TraceBlockSinkFactory {
    pub async fn new() -> eyre::Result<Self> {
        // Create a new JSON-RPC server
        let server = Server::builder()
            .set_message_buffer_capacity(5)
            .build("127.0.0.1:8547")
            .await?;

        // Create a broadcast channel
        let (tx, _rx) = broadcast::channel::<String>(16);

        let mut module = RpcModule::new(tx.clone());

        // Register a subscription method named "eth_stateDiffSubscription"
        module
            .register_subscription("eth_subscribeStateDiffs", "eth_stateDiffSubscription", "eth_unsubscribeStateDiffs", |_params, pending, ctx| async move {
                let rx = ctx.subscribe();
                let stream = BroadcastStream::new(rx);
                Self::pipe_from_stream(pending, stream).await?;
                Ok(())
            })
            .unwrap();

        // Get the server's local address
        let addr = server.local_addr()?;
        // Start the server with the configured module
        let handle = server.start(module);

        // We may use the `ServerHandle` to shut it down or manage it ourselves later
        tokio::spawn(handle.stopped());

        // Log that the subscription server has started
        info!("Block subscription server started on {}", addr);

        // Log when a new WebSocket connection is received
        info!("New WebSocket connection received");

        // Return the TraceBlockSinkFactory instance
        Ok(Self { tx })
    }

    // Method to handle sending messages from the broadcast stream to subscribers
    async fn pipe_from_stream(
        pending: PendingSubscriptionSink,
        mut stream: BroadcastStream<String>,
    ) -> Result<(), jsonrpsee::core::Error> {
        let sink = match pending.accept().await {
            Ok(sink) => sink,
            Err(e) => {
                warn!("Failed to accept subscription: {:?}", e);
                return Ok(());
            }
        };

        loop {
            tokio::select! {
                _ = sink.closed() => {
                    // connection dropped
                    break Ok(())
                },
                maybe_item = stream.next() => {
                    let item = match maybe_item {
                        Some(Ok(item)) => item,
                        Some(Err(e)) => {
                            warn!("Error in WebSocket stream: {:?}", e);
                            break Ok(());
                        },
                        None => {
                            // stream ended
                            break Ok(())
                        },
                    };
                    let msg = SubscriptionMessage::from_json(&item)?;
                    if sink.send(msg).await.is_err() {
                        break Ok(());
                    }
                }
            }
        }
    }
}

impl UnfinishedBlockBuildingSinkFactory for TraceBlockSinkFactory {
    fn create_sink(
        &mut self,
        _slot_data: MevBoostSlotData,
        _cancel: CancellationToken,
    ) -> Arc<dyn rbuilder::building::builders::UnfinishedBlockBuildingSink> {
        Arc::new(TracingBlockSink {tx: self.tx.clone()})
    }
}

#[derive(Clone, Debug)]
struct TracingBlockSink {
    tx: broadcast::Sender<String>,
}

impl UnfinishedBlockBuildingSink for TracingBlockSink {
    // This method is called when a new block has been built
    // After each block is built, we send the block number, timestamp, block uuid and the pending state to the client
    fn new_block(&self, block: Box<dyn BlockBuildingHelper>) {
        let building_context = block.building_context();
        let bundle_state = block.get_bundle_state().state();

        // Create a new StateOverride object to store the changes
        let mut pending_state = StateOverride::new();

        // Iterate through each address and account in the bundle state
        for (address, account) in bundle_state.iter() {
            let mut account_override = AccountOverride::default();

            let mut state_diff = HashMap::new();
            for (storage_key, storage_slot) in &account.storage {
                let key = B256::from(*storage_key);
                let value = B256::from(storage_slot.present_value);
                state_diff.insert(key, value);
            }

            if !state_diff.is_empty() {
                account_override.state_diff = Some(state_diff);
                pending_state.insert(*address, account_override);
            }

        }

        let block_data = json!({
            "blockNumber": building_context.block_env.number,
            "blockTimestamp": building_context.block_env.timestamp,
            "blockUuid": Uuid::new_v4().to_string(),
            "pendingState": pending_state
        });

        if let Err(e) = self.tx.send(serde_json::to_string(&block_data).unwrap()) {
            warn!("Failed to send block data: {:?}", e);
        }
        info!(
            order_count =? block.built_block_trace().included_orders.len(),
            "Block generated. Throwing it away!"
        );
    }

    fn can_use_suggested_fee_recipient_as_coinbase(&self) -> bool {
        false
    }
}

////////////////////////////
/// BUILDING ALGORITHM
////////////////////////////

/// Dummy algorithm that waits for some orders and creates a block inserting them in the order they arrived.
/// Generates only a single block.
/// This is a NOT real builder some data is not filled correctly (eg:BuiltBlockTrace)
#[derive(Debug)]
struct DummyBuildingAlgorithm {
    /// Amnount of used orders to build a block
    orders_to_use: usize,
    root_hash_task_pool: BlockingTaskPool,
}

const ORDER_POLLING_PERIOD: Duration = Duration::from_millis(10);
const BUILDER_NAME: &str = "DUMMY";
impl DummyBuildingAlgorithm {
    pub fn new(orders_to_use: usize) -> Self {
        Self {
            orders_to_use,
            root_hash_task_pool: BlockingTaskPool::new(
                BlockingTaskPool::builder().num_threads(1).build().unwrap(),
            ),
        }
    }

    fn wait_for_orders(
        &self,
        cancel: &CancellationToken,
        orders_source: broadcast::Receiver<SimulatedOrderCommand>,
    ) -> Option<Vec<SimulatedOrder>> {
        let mut orders_sink = SimulatedOrderStore::new();
        let mut order_consumer = OrderConsumer::new(orders_source);
        loop {
            if cancel.is_cancelled() {
                break None;
            }
            order_consumer.consume_next_commands().unwrap();
            order_consumer.apply_new_commands(&mut orders_sink);
            let orders = orders_sink.get_orders();
            if orders.len() >= self.orders_to_use {
                break Some(orders);
            }
            sleep(ORDER_POLLING_PERIOD);
        }
    }

    fn build_block<DB: Database + Clone + 'static>(
        &self,
        orders: Vec<SimulatedOrder>,
        provider_factory: ProviderFactory<DB>,
        ctx: &BlockBuildingContext,
    ) -> eyre::Result<Box<dyn BlockBuildingHelper>> {
        let mut block_building_helper = BlockBuildingHelperFromDB::new(
            provider_factory.clone(),
            self.root_hash_task_pool.clone(),
            RootHashConfig::live_config(false, false),
            ctx.clone(),
            None,
            BUILDER_NAME.to_string(),
            false,
            None,
            CancellationToken::new(),
        )?;

        for order in orders {
            // don't care about the result
            let _ = block_building_helper.commit_order(&order)?;
        }
        Ok(Box::new(block_building_helper))
    }
}

impl<DB: Database + Clone + 'static> BlockBuildingAlgorithm<DB> for DummyBuildingAlgorithm {
    fn name(&self) -> String {
        BUILDER_NAME.to_string()
    }

    fn build_blocks(&self, input: BlockBuildingAlgorithmInput<DB>) {
        if let Some(orders) = self.wait_for_orders(&input.cancel, input.input) {
            let block = self
                .build_block(orders, input.provider_factory, &input.ctx)
                .unwrap();
            input.sink.new_block(block);
        }
    }
}
