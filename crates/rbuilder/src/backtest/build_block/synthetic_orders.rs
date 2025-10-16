use alloy_primitives::B256;
use clap::{command, Parser};
use rbuilder_config::load_toml_config;
use rbuilder_primitives::{
    Bundle, MempoolTx, Metadata, Order, TransactionSignedEcRecoveredWithBlobs, LAST_BUNDLE_VERSION,
};
use reth_provider::test_utils::MockNodeTypesWithDB;
use uuid::Uuid;

use super::backtest_build_block::{run_backtest_build_block, BuildBlockCfg, OrdersSource};
use crate::{
    backtest::OrdersWithTimestamp,
    building::{
        testing::test_chain_state::{BlockArgs, NamedAddr, TestChainState, TxArgs},
        BlockBuildingContext,
    },
    live_builder::cli::LiveBuilderConfig,
    provider::state_provider_factory_from_provider_factory::StateProviderFactoryFromProviderFactory,
};

#[derive(Parser, Debug)]
struct ExtraCfg {
    #[clap(long, help = "Tx count")]
    tx_count: u64,
    #[clap(long, help = "Bundle count")]
    bundle_count: u64,
}

#[derive(Parser, Debug)]
struct Cli {
    #[command(flatten)]
    pub build_block_cfg: BuildBlockCfg,
    #[command(flatten)]
    pub extra_cfg: ExtraCfg,
}

/// OrdersSource using a fake chain state and some synthetic orders.
/// Creates 2 types of orders:
/// - Tx: a tx from user 1 paying a small value to coinbase.
/// - Bundle: a tx from user 2 paying a small value to coinbase followed by another tx from user 3 paying a large value to coinbase.
///
/// All the nonces are properly set so all the txs and bundles are executable (in the correct order).
struct SyntheticOrdersSource<ConfigType> {
    test_chain_state: TestChainState,
    orders: Vec<OrdersWithTimestamp>,
    config: ConfigType,
}

const LOW_TIP: u64 = 1_000_000;
const HIGH_TIP: u64 = 1_000_000_000;

/// Creates a TransactionSignedEcRecoveredWithBlobs from user paying tip to coinbase.
fn create_tip_tx(
    test_chain_state: &TestChainState,
    user: usize,
    nonce: u64,
    tip: u64,
) -> TransactionSignedEcRecoveredWithBlobs {
    let tx_args = TxArgs::new_send_to_coinbase(NamedAddr::User(user), nonce, tip);
    let tx = test_chain_state.sign_tx(tx_args).unwrap();
    TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx).unwrap()
}

impl<ConfigType: LiveBuilderConfig> SyntheticOrdersSource<ConfigType> {
    fn new(extra_cfg: ExtraCfg, config: ConfigType) -> eyre::Result<Self> {
        let block_number = 1;
        let test_chain_state = TestChainState::new(BlockArgs::default().number(block_number))?;
        let mut orders = Vec::new();
        for i in 0..extra_cfg.tx_count {
            let order = Order::Tx(MempoolTx::new(create_tip_tx(
                &test_chain_state,
                1,
                i,
                LOW_TIP,
            )));
            orders.push(OrdersWithTimestamp {
                timestamp_ms: 0,
                order,
            });
        }

        for i in 0..extra_cfg.bundle_count {
            let low_tip_tx = create_tip_tx(&test_chain_state, 2, i, LOW_TIP);
            let high_tip_tx = create_tip_tx(&test_chain_state, 3, i, HIGH_TIP);
            let mut bundle = Bundle {
                block: Some(block_number),
                min_timestamp: None,
                max_timestamp: None,
                txs: vec![low_tip_tx, high_tip_tx],
                reverting_tx_hashes: Default::default(),
                hash: B256::ZERO,
                uuid: Uuid::nil(),
                replacement_data: None,
                signer: None,
                refund_identity: None,
                metadata: Metadata {
                    received_at_timestamp: time::OffsetDateTime::from_unix_timestamp(0).unwrap(),
                    is_system: false,
                    refund_identity: None,
                },
                dropping_tx_hashes: Default::default(),
                refund: None,
                version: LAST_BUNDLE_VERSION,
                external_hash: None,
            };
            bundle.hash_slow();
            orders.push(OrdersWithTimestamp {
                timestamp_ms: 0,
                order: Order::Bundle(bundle),
            });
        }

        Ok(Self {
            orders,
            test_chain_state,
            config,
        })
    }
}

impl<ConfigType: LiveBuilderConfig>
    OrdersSource<ConfigType, StateProviderFactoryFromProviderFactory<MockNodeTypesWithDB>>
    for SyntheticOrdersSource<ConfigType>
{
    fn available_orders(&self) -> Vec<OrdersWithTimestamp> {
        self.orders.clone()
    }

    fn block_time_as_unix_ms(&self) -> u64 {
        0
    }

    fn create_provider_factory(
        &self,
    ) -> eyre::Result<StateProviderFactoryFromProviderFactory<MockNodeTypesWithDB>> {
        Ok(StateProviderFactoryFromProviderFactory::new(
            self.test_chain_state.provider_factory().clone(),
            None,
        ))
    }

    fn create_block_building_context(&self) -> eyre::Result<BlockBuildingContext> {
        Ok(self.test_chain_state.block_building_context().clone())
    }

    fn print_custom_stats(
        &self,
        _provider: StateProviderFactoryFromProviderFactory<MockNodeTypesWithDB>,
    ) -> eyre::Result<()> {
        Ok(())
    }

    fn config(&self) -> &ConfigType {
        &self.config
    }
}

pub async fn run_backtest<ConfigType: LiveBuilderConfig>() -> eyre::Result<()> {
    let cli = Cli::parse();
    let config: ConfigType = load_toml_config(cli.build_block_cfg.config.clone())?;
    let order_source = SyntheticOrdersSource::new(cli.extra_cfg, config)?;
    run_backtest_build_block(cli.build_block_cfg, order_source).await
}
