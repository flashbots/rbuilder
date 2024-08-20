//! Application to test the orders input + simulation.
//! For each block it subscribes an [`OrderReplacementManager`].
//! Since simulation needs to pull orders, the [`OrderReplacementManager`] is adapted with an [`OrderSender2OrderSink`] generating an [`OrdersForBlock`] for
//! the simulation stage to pull.

use alloy_primitives::utils::format_ether;
use clap::Parser;
use jsonrpsee::RpcModule;
use payload_events::MevBoostSlotDataGenerator;
use rbuilder::{
    building::BlockBuildingContext,
    live_builder::{
        base_config::load_config_toml_and_env,
        cli::LiveBuilderConfig,
        config::Config,
        order_input::{
            order_replacement_manager::OrderReplacementManager, orderpool::OrdersForBlock,
            start_orderpool_jobs, OrderInputConfig,
        },
        payload_events,
        simulation::{OrderSimulationPool, SimulatedOrderCommand},
    },
    telemetry::spawn_telemetry_server,
    utils::{build_info::rbuilder_version, Signer},
};
use reth::providers::HeaderProvider;
use std::{path::PathBuf, time::Duration};
use tokio::signal::ctrl_c;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(Parser, Debug)]
struct Cli {
    #[clap(long, help = "path to output csv file")]
    csv: Option<PathBuf>,
    #[clap(long, help = "maximum blocks to run", default_value = "1000000")]
    max_blocks: u64,
    #[clap(help = "Config file path")]
    config: PathBuf,
}

#[tokio::main]
pub async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    let config: Config = load_config_toml_and_env(cli.config)?;
    config.base_config().setup_tracing_subsriber()?;

    let cancel = CancellationToken::new();

    spawn_telemetry_server(config.base_config().telemetry_address(), rbuilder_version()).await?;

    let relays = config.l1_config.create_relays()?;

    let (_new_slots, mut slots) = MevBoostSlotDataGenerator::new(
        config.l1_config.beacon_clients()?,
        relays,
        Default::default(),
        CancellationToken::new(),
    )
    .spawn();

    let provider_factory = config.base_config().provider_factory()?;

    let order_input_config = OrderInputConfig::from_config(config.base_config());

    let (_orderpool, order_pool_subscriber) = start_orderpool_jobs(
        order_input_config,
        provider_factory.clone(),
        RpcModule::new(()),
        cancel.clone(),
    )
    .await?;

    let sim_pool = OrderSimulationPool::new(provider_factory.clone(), 4, cancel.clone());

    let mut current_slot = if let Some(slot) = slots.recv().await {
        slot
    } else {
        return Ok(());
    };

    let mut total_slots = 0;
    let mut orders_last_slot = 0;
    'slots: loop {
        let provider_factory = provider_factory.provider_factory_unchecked();
        info!(
            "Current slot: {}, sim_orders_prev_slot: {}",
            current_slot.payload_attributes_event.data.proposal_slot, orders_last_slot,
        );
        if total_slots > cli.max_blocks {
            break;
        }
        total_slots += 1;
        orders_last_slot = 0;

        let block_number = current_slot
            .payload_attributes_event
            .data
            .parent_block_number
            + 1;
        // Orders sent to the sink will be polled on orders_for_block.
        let (orders_for_block, sink) = OrdersForBlock::new_with_sink();
        // Add OrderReplacementManager to manage replacements and cancellations.
        let order_replacement_manager = OrderReplacementManager::new(Box::new(sink));
        let _block_sub = order_pool_subscriber
            .add_sink_auto_remove(block_number, Box::new(order_replacement_manager));
        let mut sleeps = 0;
        let parent_header = loop {
            if let Some(header) = provider_factory
                .header(&current_slot.payload_attributes_event.data.parent_block_hash)?
            {
                break header;
            } else {
                if sleeps > 20 {
                    warn!("Can't get header for slot");
                    continue 'slots;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                sleeps += 1;
            }
        };

        let block_ctx = BlockBuildingContext::from_attributes(
            current_slot.payload_attributes_event.clone(),
            &parent_header,
            Signer::random(),
            config.base_config().chain_spec()?,
            Default::default(),
            None,
            Vec::new(),
            None,
        );

        let block_cancel = CancellationToken::new();
        let mut sim_results =
            sim_pool.spawn_simulation_job(block_ctx, orders_for_block, block_cancel.clone());
        loop {
            tokio::select! {
                new_slot = slots.recv() => {
                    if let Some(new_slot) = new_slot {
                        current_slot = new_slot;
                        block_cancel.cancel();
                        continue 'slots;
                    } else {
                        info!("Slots channel closed");
                        break 'slots;
                    }
                },
                sim_command = sim_results.orders.recv() => {
                    if let Some(sim_command) = sim_command {
                        match sim_command{
                            SimulatedOrderCommand::Simulation(sim) => {
                                orders_last_slot += 1;
                                info!(order_id = ?sim.order.id(),value = ? format_ether(sim.sim_value.coinbase_profit),"Simulated order");
                            },
                            SimulatedOrderCommand::Cancellation(id) => {
                                info!(order_id = ?id,"Cancelled  order");

                            },
                        }
                    } else {
                        warn!("sim results channel closed");
                        block_cancel.cancel();
                        continue 'slots;
                    }
                }
                _ = ctrl_c() => {
                    break 'slots;
                }
            }
        }
    }

    Ok(())
}
