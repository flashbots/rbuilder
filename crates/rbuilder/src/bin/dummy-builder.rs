//! This simple app shows how to run a custom block builder.
//! It uses no bidding strategy, it just bids all available profit.
//! It does not sends blocks to any relay, it just logs the generated blocks.
//! The algorithm is really dummy, it just adds some txs it receives and generates a single block.
//! This is NOT intended to be run in production so it has no nice configuration, poor error checking and some hardcoded values.
use std::{path::PathBuf, sync::Arc, thread::sleep, time::Duration};

use clap::{Parser, ValueEnum};
use jsonrpsee::RpcModule;
use rbuilder::live_builder::config::Config;
use rbuilder::{
    beacon_api_client::Client,
    building::{
        builders::{
            block_building_helper::{
                BiddableUnfinishedBlock, BlockBuildingHelper, BlockBuildingHelperFromProvider,
            },
            BlockBuildingAlgorithm, BlockBuildingAlgorithmInput, OrderConsumer,
            UnfinishedBlockBuildingSink, UnfinishedBlockBuildingSinkFactory,
        },
        BlockBuildingContext, SimulatedOrderStore, ThreadBlockBuildingContext,
    },
    live_builder::{
        base_config::{
            default_ip, load_config_toml_and_env, DEFAULT_EL_NODE_IPC_PATH,
            DEFAULT_INCOMING_BUNDLES_PORT, DEFAULT_RETH_DB_PATH,
        },
        block_list_provider::NullBlockListProvider,
        config::create_provider_factory,
        order_input::{
            MempoolSource, OrderInputConfig, DEFAULT_INPUT_CHANNEL_BUFFER_SIZE,
            DEFAULT_RESULTS_CHANNEL_TIMEOUT, DEFAULT_SERVE_MAX_CONNECTIONS,
        },
        payload_events::{MevBoostSlotData, MevBoostSlotDataGenerator},
        simulation::SimulatedOrderCommand,
        LiveBuilder,
    },
    mev_boost::RelayClient,
    preconf::PreconfConfig,
    primitives::{mev_boost::MevBoostRelaySlotInfoProvider, SimulatedOrder},
    provider::StateProviderFactory,
    utils::{ProviderFactoryReopener, Signer},
};
use reth_chainspec::MAINNET;
use reth_db::DatabaseEnv;
use reth_node_api::NodeTypesWithDBAdapter;
use reth_node_ethereum::EthereumNode;
use tokio::{
    signal::ctrl_c,
    sync::{broadcast, mpsc},
};
use tokio_util::sync::CancellationToken;
use tracing::{info, level_filters::LevelFilter};

const RETH_DB_PATH: &str = "/home/ubuntu/reth/data";
#[derive(Parser)]
struct Cli {
    #[arg(long, help = "path to output csv file")]
    csv: Option<PathBuf>,
    #[arg(long, help = "maximum blocks to run", default_value = "1000000")]
    max_blocks: u64,
    #[arg(help = "Config file path")]
    config: PathBuf,
}
#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    let config: Config = load_config_toml_and_env(cli.config)?;
    let chain_spec = config.base_config.chain_spec()?;

    let env =
        tracing_subscriber::EnvFilter::from_default_env().add_directive(LevelFilter::INFO.into());
    let writer = tracing_subscriber::fmt()
        .with_env_filter(env)
        .with_test_writer();
    writer.init();
    let cancel = CancellationToken::new();

    let flashbots_relay_url = "https://0xafa4c6985aa049fb79dd37010438cfebeb0f2bd42b115b89dd678dab0670c1de38da0c4e9138c9290a398ecd9a0b3110@boost-relay-sepolia.flashbots.net";
    let relay_client = RelayClient::from_url(flashbots_relay_url.parse()?, None, None, None);
    let relay = MevBoostRelaySlotInfoProvider::new(relay_client, "flashbots".to_string());
    let blocklist_provider = Arc::new(NullBlockListProvider::new());
    let payload_event = MevBoostSlotDataGenerator::new(
        config.l1_config.beacon_clients()?,
        vec![relay],
        blocklist_provider.clone(),
        cancel.clone(),
    );

    let order_input_config = OrderInputConfig::new(
        false,
        true,
        Some(MempoolSource::Ipc(PathBuf::from(DEFAULT_EL_NODE_IPC_PATH))),
        DEFAULT_INCOMING_BUNDLES_PORT,
        default_ip(),
        DEFAULT_SERVE_MAX_CONNECTIONS,
        DEFAULT_RESULTS_CHANNEL_TIMEOUT,
        DEFAULT_INPUT_CHANNEL_BUFFER_SIZE,
    );
    let preconf_config = PreconfConfig::from_config(&config);
    let (orderpool_sender, orderpool_receiver) =
        mpsc::channel(order_input_config.input_channel_buffer_size);
    let builder = LiveBuilder::<
        ProviderFactoryReopener<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>,
        MevBoostSlotDataGenerator,
    > {
        watchdog_timeout: Some(Duration::from_secs(10000)),
        error_storage_path: None,
        simulation_threads: 1,
        blocks_source: payload_event,
        order_input_config,
        chain_chain_spec: chain_spec.clone(),
        provider: create_provider_factory(
            Some(&RETH_DB_PATH.parse::<PathBuf>().unwrap()),
            None,
            None,
            chain_spec.clone(),
            None,
        )?,
        coinbase_signer: Signer::random(),
        extra_data: Vec::new(),
        blocklist_provider,
        global_cancellation: cancel.clone(),
        extra_rpc: RpcModule::new(()),
        sink_factory: Box::new(TraceBlockSinkFactory {}),
        builders: vec![Arc::new(DummyBuildingAlgorithm::new(10))],
        run_sparse_trie_prefetcher: false,
        orderpool_sender,
        orderpool_receiver,
        sbundle_merger_selected_signers: Default::default(),
        preconf_config,
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
    fn new_block(&self, block: BiddableUnfinishedBlock) {
        info!(
            order_count =? block.block().built_block_trace().included_orders.len(),
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
    /// Amount of used orders to build a block
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
    ) -> Option<Vec<Arc<SimulatedOrder>>> {
        let mut orders_sink = SimulatedOrderStore::new();
        let mut order_consumer = OrderConsumer::new(orders_source);
        loop {
            if cancel.is_cancelled() {
                break None;
            }
            order_consumer.blocking_consume_next_commands().unwrap();
            order_consumer.apply_new_commands(&mut orders_sink);
            let orders = orders_sink.get_orders();
            if orders.len() >= self.orders_to_use {
                break Some(orders);
            }
            sleep(ORDER_POLLING_PERIOD);
        }
    }

    fn build_block<P>(
        &self,
        orders: Vec<Arc<SimulatedOrder>>,
        provider: P,
        ctx: &BlockBuildingContext,
    ) -> eyre::Result<Box<dyn BlockBuildingHelper>>
    where
        P: StateProviderFactory + Clone + 'static,
    {
        let mut local_ctx = ThreadBlockBuildingContext::default();
        let block_state = provider
            .history_by_block_hash(ctx.attributes.parent)?
            .into();

        let mut block_building_helper = BlockBuildingHelperFromProvider::new(
            block_state,
            ctx.clone(),
            &mut local_ctx,
            BUILDER_NAME.to_string(),
            false,
            0,
            false,
            CancellationToken::new(),
        )?;

        for order in orders {
            // don't care about the result
            let _ = block_building_helper.commit_order(&mut local_ctx, &order, &|_| Ok(()))?;
        }
        Ok(Box::new(block_building_helper))
    }
}

impl<P> BlockBuildingAlgorithm<P> for DummyBuildingAlgorithm
where
    P: StateProviderFactory + Clone + 'static,
{
    fn name(&self) -> String {
        BUILDER_NAME.to_string()
    }

    fn build_blocks(&self, input: BlockBuildingAlgorithmInput<P>) {
        if let Some(orders) = self.wait_for_orders(&input.cancel, input.input) {
            let block = self
                .build_block(orders, input.provider, &input.ctx)
                .unwrap();
            if let Ok(block) = BiddableUnfinishedBlock::new(block) {
                input.sink.new_block(block);
            }
        }
    }
}
