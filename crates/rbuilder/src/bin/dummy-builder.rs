//! This simple app shows how to run a custom block builder.
//! It uses no bidding strategy, it just bids all available profit.
//! It does not sends blocks to any relay, it just logs the generated blocks.
//! The algorithm is really dummy, it just adds some txs it receives and generates a single block.
//! This is NOT intended to be run in production so it has no nice configuration, poor error checking and some hardcoded values.
use std::{path::PathBuf, sync::Arc, thread::sleep, time::Duration};

use alloy_primitives::U256;
use jsonrpsee::RpcModule;
use rbuilder::{
    beacon_api_client::Client,
    building::{
        builders::{
            finalize_block_execution, Block, BlockBuildingAlgorithm, BlockBuildingAlgorithmInput,
            BlockBuildingSink, BuilderSinkFactory, OrderConsumer,
        },
        BlockBuildingContext, BlockState, BuiltBlockTrace, PartialBlock, SimulatedOrderStore,
    },
    live_builder::{
        base_config::{
            DEFAULT_EL_NODE_IPC_PATH, DEFAULT_ERROR_STORAGE_PATH, DEFAULT_INCOMING_BUNDLES_PORT,
            DEFAULT_IP, DEFAULT_RETH_DB_PATH,
        },
        bidding::{DummyBiddingService, SlotBidder},
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
    roothash::RootHashMode,
    utils::Signer,
};
use reth::{providers::ProviderFactory, tasks::pool::BlockingTaskPool};
use reth_chainspec::MAINNET;
use reth_db::{database::Database, DatabaseEnv};
use tokio::{signal::ctrl_c, sync::broadcast};
use tokio_util::sync::CancellationToken;
use tracing::{info, level_filters::LevelFilter};

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
    let bidding_service = Box::new(DummyBiddingService {});

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

    let builder = LiveBuilder::<Arc<DatabaseEnv>, TraceBlockSinkFactory, MevBoostSlotDataGenerator> {
        watchdog_timeout: Duration::from_secs(10000),
        error_storage_path: DEFAULT_ERROR_STORAGE_PATH.parse().unwrap(),
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
        bidding_service,
        extra_rpc: RpcModule::new(()),
        sink_factory: TraceBlockSinkFactory {},
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
/// BLOCK SINK
/////////////////////////
struct TraceBlockSinkFactory {}

impl BuilderSinkFactory for TraceBlockSinkFactory {
    type SinkType = TracingBlockSink;

    fn create_builder_sink(
        &self,
        _: MevBoostSlotData,
        _: Arc<dyn SlotBidder>,
        _: CancellationToken,
    ) -> TracingBlockSink {
        TracingBlockSink {}
    }
}

#[derive(Clone, Debug)]
struct TracingBlockSink {}

impl BlockBuildingSink for TracingBlockSink {
    fn new_block(&self, block: Block) {
        info!(
            tx_count =? block.sealed_block.body.len(),
            "Block generated. Throwing it away!"
        );
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
        bidder: &dyn SlotBidder,
    ) -> eyre::Result<Option<Block>> {
        let mut partial_block = PartialBlock::new(false, None);
        let state_provider = provider_factory.history_by_block_hash(ctx.attributes.parent)?;
        let mut state = BlockState::new(&state_provider);

        partial_block.pre_block_call(ctx, &mut state)?;
        for order in orders {
            // don't care about the result
            let _ = partial_block.commit_order(&order, ctx, &mut state)?;
        }
        let mut fake_trace = BuiltBlockTrace::new();
        let should_finalize = finalize_block_execution(
            ctx,
            &mut partial_block,
            &mut state,
            &mut fake_trace,
            None,
            bidder,
            U256::from(0),
        )?;
        if !should_finalize {
            return Ok(None);
        }
        let finalized_block = partial_block.finalize(
            state,
            ctx,
            provider_factory.clone(),
            RootHashMode::CorrectRoot,
            self.root_hash_task_pool.clone(),
        )?;
        Ok(Some(Block {
            trace: fake_trace,
            sealed_block: finalized_block.sealed_block,
            txs_blobs_sidecars: finalized_block.txs_blob_sidecars,
            builder_name: BUILDER_NAME.to_string(),
        }))
    }
}

impl<DB: Database + Clone + 'static, SinkType: BlockBuildingSink>
    BlockBuildingAlgorithm<DB, SinkType> for DummyBuildingAlgorithm
{
    fn name(&self) -> String {
        BUILDER_NAME.to_string()
    }

    fn build_blocks(&self, input: BlockBuildingAlgorithmInput<DB, SinkType>) {
        if let Some(orders) = self.wait_for_orders(&input.cancel, input.input) {
            if let Some(block) = self
                .build_block(
                    orders,
                    input.provider_factory,
                    &input.ctx,
                    input.slot_bidder.as_ref(),
                )
                .unwrap()
            {
                input.sink.new_block(block);
            }
        }
    }
}
