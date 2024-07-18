//! This simple app shows how to run a simulation on a TestChainState without the need of any external CL or EL
//! This could evolve into just a test case

use rbuilder::{
    building::testing::test_chain_state::{BlockArgs, NamedAddr, TestChainState, TxArgs},
    live_builder::{
        order_input::{order_sink::OrderPoolCommand, orderpool::OrdersForBlock},
        simulation::{OrderSimulationPool, SimulatedOrderCommand},
    },
    primitives::{MempoolTx, Order, TransactionSignedEcRecoveredWithBlobs},
    utils::ProviderFactoryReopener,
};
use reth_primitives::U256;
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

    // Create simulation core
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

    // Create a simple tx that sends to coinbase 5 wei.
    let coinbase_profit = 5;
    // max_priority_fee will be 0
    let tx_args = TxArgs::new_send_to_coinbase(NamedAddr::User(1), 0, coinbase_profit);
    let tx = test_context.sign_tx(tx_args)?;
    let tx = TransactionSignedEcRecoveredWithBlobs::new_no_blobs(tx).unwrap();
    order_sender.send(OrderPoolCommand::Insert(Order::Tx(MempoolTx::new(tx))))?;
    // We expect to receive the simulation giving a profit of coinbase_profit since that's what we sent directly to coinbase.
    // and we are not paying any priority fee
    if let Some(command) = sim_results.orders.recv().await {
        match command {
            SimulatedOrderCommand::Simulation(sim_order) => {
                println!("Got the sim order!! SimValue = {:?}", sim_order.sim_value);
                assert_eq!(
                    sim_order.sim_value.coinbase_profit,
                    U256::from(coinbase_profit)
                );
            }
            SimulatedOrderCommand::Cancellation(_) => panic!("Cancellation not expected"),
        };
    }

    Ok(())
}
