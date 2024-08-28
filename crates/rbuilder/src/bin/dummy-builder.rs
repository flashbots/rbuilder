//! This simple app shows how to run a custom block builder.
//! It uses no bidding strategy, it just bids all available profit.
//! It does not sends blocks to any relay, it just logs the generated blocks.
//! The algorithm is really dummy, it just adds some txs it receives and generates a single block.
//! This is NOT intended to be run in production so it has no nice configuration, poor error checking and some hardcoded values.
use std::{path::PathBuf, sync::Arc, thread::sleep, time::Duration};

use jsonrpsee::RpcModule;
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
            create_provider_factory, DEFAULT_EL_NODE_IPC_PATH, DEFAULT_ERROR_STORAGE_PATH,
            DEFAULT_INCOMING_BUNDLES_PORT, DEFAULT_IP, DEFAULT_RETH_DB_PATH,
        },
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
    provider::StateProviderFactory,
    roothash::RootHashMode,
    utils::{ProviderFactoryReopener, Signer},
};
use reth_chainspec::MAINNET;
use reth_db::DatabaseEnv;
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

    let builder =
        LiveBuilder::<ProviderFactoryReopener<Arc<DatabaseEnv>>, MevBoostSlotDataGenerator> {
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
                true,
                1,
            )?,
            coinbase_signer: Signer::random(),
            extra_data: Vec::new(),
            blocklist: Default::default(),
            global_cancellation: cancel.clone(),
            extra_rpc: RpcModule::new(()),
            sink_factory: Box::new(TraceBlockSinkFactory {}),
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
#[derive(Debug)]
struct TraceBlockSinkFactory {}

impl UnfinishedBlockBuildingSinkFactory for TraceBlockSinkFactory {
    fn create_sink(
        &mut self,
        _slot_data: MevBoostSlotData,
        _cancel: CancellationToken,
    ) -> Arc<dyn rbuilder::building::builders::UnfinishedBlockBuildingSink> {
        Arc::new(TracingBlockSink {})
    }
}

#[derive(Clone, Debug)]
struct TracingBlockSink {}

impl UnfinishedBlockBuildingSink for TracingBlockSink {
    fn new_block(&self, block: Box<dyn BlockBuildingHelper>) {
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
}

const ORDER_POLLING_PERIOD: Duration = Duration::from_millis(10);
const BUILDER_NAME: &str = "DUMMY";
impl DummyBuildingAlgorithm {
    pub fn new(orders_to_use: usize) -> Self {
        Self { orders_to_use }
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

    fn build_block<Provider: StateProviderFactory + Clone + 'static>(
        &self,
        orders: Vec<SimulatedOrder>,
        provider_factory: Provider,
        ctx: &BlockBuildingContext,
    ) -> eyre::Result<Box<dyn BlockBuildingHelper>> {
        let mut block_building_helper = BlockBuildingHelperFromDB::new(
            provider_factory.clone(),
            RootHashMode::CorrectRoot,
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

impl<Provider: StateProviderFactory + Clone + 'static> BlockBuildingAlgorithm<Provider>
    for DummyBuildingAlgorithm
{
    fn name(&self) -> String {
        BUILDER_NAME.to_string()
    }

    fn build_blocks(&self, input: BlockBuildingAlgorithmInput<Provider>) {
        if let Some(orders) = self.wait_for_orders(&input.cancel, input.input) {
            let block = self
                .build_block(orders, input.provider_factory, &input.ctx)
                .unwrap();
            input.sink.new_block(block);
        }
    }
}
