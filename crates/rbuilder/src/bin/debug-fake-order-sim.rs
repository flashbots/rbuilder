use std::time::Duration;

use rbuilder::{
    building::testing::test_chain_state::{BlockArgs, NamedAddr, TestChainState, TxArgs},
    live_builder::{
        order_input::{order_sink::OrderPoolCommand, orderpool::OrdersForBlock},
        simulation::OrderSimulationPool,
    },
    primitives::{MempoolTx, Order, TransactionSignedEcRecoveredWithBlobs},
    utils::ProviderFactoryReopener,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Sample app that creates a spawn_simulation_job and simulates a fake order.
#[tokio::main]
pub async fn main() -> eyre::Result<()> {
    let env = tracing_subscriber::EnvFilter::from_default_env();
    let writer = tracing_subscriber::fmt()
        .with_env_filter(env)
        .with_test_writer();
    writer.init();

    let test_context = TestChainState::new(BlockArgs::default().number(11))?;

    let cancel = CancellationToken::new();
    let provider_factory_reopener = ProviderFactoryReopener::new_from_existing_for_testing(
        test_context.provider_factory().clone(),
    )?;
    let sim_pool = OrderSimulationPool::new(provider_factory_reopener, 4, cancel.clone());
    let (order_sender, order_receiver) = mpsc::unbounded_channel();
    let orders_for_block = OrdersForBlock {
        new_order_sub: order_receiver,
    };

    let mut sim_results = sim_pool.spawn_simulation_job(
        test_context.block_building_context().clone(),
        orders_for_block,
        cancel.clone(),
    );

    tokio::spawn(async move {
        while let Some(command) = sim_results.orders.recv().await {
            println!("{:?}", command);
        }
    });

    let tx_args = TxArgs::new_send_to_coinbase(NamedAddr::User(1), 0, 5);
    let tx = test_context.sign_tx(tx_args)?;
    let tx = TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx).unwrap();
    order_sender.send(OrderPoolCommand::Insert(Order::Tx(MempoolTx::new(tx))))?;

    tokio::time::sleep(Duration::from_secs(5)).await;

    Ok(())
}
