//! This simple app shows how to get the slots from the CL client via a MevBoostSlotDataGenerator

use clap::Parser;
use payload_events::MevBoostSlotDataGenerator;
use rbuilder::{
    live_builder::{
        base_config::load_config_toml_and_env, cli::LiveBuilderConfig, config::Config,
        payload_events,
    },
    telemetry::spawn_telemetry_server,
    utils::{build_info::rbuilder_version, unix_timestamp_now},
};
use std::path::PathBuf;
use tokio::signal::ctrl_c;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Parser, Debug)]
struct Cli {
    #[clap(help = "Config file path")]
    config: PathBuf,
}

#[tokio::main]
pub async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    let config: Config = load_config_toml_and_env(cli.config)?;
    config.base_config().setup_tracing_subsriber()?;

    let cancel = CancellationToken::new();

    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        ctrl_c().await.unwrap_or_default();
        cancel_clone.cancel();
    });

    spawn_telemetry_server(config.base_config().telemetry_address(), rbuilder_version()).await?;

    let relays = config.l1_config.relays()?;

    let (handle, mut slots) = MevBoostSlotDataGenerator::new(
        config.l1_config.beacon_clients()?,
        relays,
        Default::default(),
        cancel,
    )
    .spawn();

    while let Some(slot) = slots.recv().await {
        let slot_number = slot.payload_attributes_event.data.proposal_slot;
        let parent_block = slot.payload_attributes_event.data.parent_block_number;
        let fee_recipient = slot
            .payload_attributes_event
            .data
            .payload_attributes
            .suggested_fee_recipient;
        let timestamp = slot
            .payload_attributes_event
            .data
            .payload_attributes
            .timestamp as i128;
        let now_ts = unix_timestamp_now() as i128;
        let suggested_gas_limit = slot.suggested_gas_limit;
        info!(
            "slot: {}, parent_block: {}, fee_recipient: {}, timestamp: {}, time_left: {}, suggested_gas_limit: {}",
            slot_number, parent_block, fee_recipient, timestamp, timestamp - now_ts, suggested_gas_limit
        );
    }

    handle.await?;
    Ok(())
}
