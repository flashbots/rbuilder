use crate::validation_api_client::ValidationAPIClient;
use ahash::HashMap;
use metrics::spawn_metrics_server;
use rbuilder::{
    beacon_api_client::Client,
    mev_boost::{MevBoostRelaySlotInfoProvider, RelayClient},
};
use rbuilder_config::LoggerConfig;
use relay::spawn_relay_server;
use std::net::SocketAddr;
use tokio_util::sync::CancellationToken;
use url::Url;

use clap::Parser;
use tokio::signal::ctrl_c;

pub mod metrics;
pub mod relay;
pub mod validation_api_client;

#[derive(Parser, Debug)]
struct Cli {
    #[clap(
        short,
        long,
        help = "API listen address",
        default_value = "0.0.0.0:80",
        env = "LISTEN_ADDRESS"
    )]
    listen_address: SocketAddr,
    #[clap(
        short,
        long,
        help = "metrics API",
        default_value = "0.0.0.0:6069",
        env = "METRICS_LISTEN_ADDRESS"
    )]
    metrics_address: SocketAddr,
    #[clap(long, action, default_value = "false", env = "LOG_JSON")]
    log_json: bool,
    #[clap(
        long,
        help = "Rust log describton",
        default_value = "info",
        env = "RUST_LOG"
    )]
    rust_log: String,
    #[clap(
        long,
        help = "URL to validate submitted blocks",
        env = "VALIDATION_URL"
    )]
    validation_url: Option<String>,
    #[clap(
        long,
        help = "Relay to fetch current epoch data",
        env = "MEV_BOOST_RELAY"
    )]
    relay: String,
    #[clap(
        long,
        help = "CL clients to fetch mev boost slot data",
        env = "CL_CLIENTS",
        value_delimiter = ',',
        value_parser
    )]
    cl_clients: Vec<String>,
    #[clap(
        long,
        help = "Map builder relay key to name, e.g. abb3..ca6c:staging-01",
        env = "BUILDER_NAMES",
        value_delimiter = ',',
        value_parser
    )]
    builder_names: Vec<String>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    let global_cancellation = CancellationToken::new();

    let logger_config = LoggerConfig {
        env_filter: cli.rust_log,
        log_json: cli.log_json,
        log_color: false,
    };
    logger_config.init_tracing()?;

    spawn_metrics_server(cli.metrics_address);

    let cl_clients = cli
        .cl_clients
        .iter()
        .map(|c| {
            let url = c.parse()?;
            Ok(Client::new(url))
        })
        .collect::<eyre::Result<Vec<_>>>()?;

    let relay = {
        let url: Url = cli.relay.parse()?;
        let client = RelayClient::from_url(
            url,
            None,
            None,
            None,
            false,
            Vec::new(),
            false,
            false,
            false,
        );
        MevBoostRelaySlotInfoProvider::new(client, "relay".to_string())
    };

    let validation_client = if let Some(url) = cli.validation_url {
        Some(ValidationAPIClient::new(&[&url])?)
    } else {
        None
    };

    let builder_names = {
        let mut map = HashMap::default();
        for arg in cli.builder_names {
            let arg: Vec<_> = arg.split(':').collect();
            if arg.len() != 2 {
                eyre::bail!("Expected builder name with format \"[<pubkey_id>]:<name>\" (e.g. \"abb3..ca6c:staging-01\"");
            }
            map.insert(arg[0].to_string(), arg[1].to_string());
        }
        map
    };

    spawn_relay_server(
        cli.listen_address,
        validation_client,
        cl_clients,
        relay,
        builder_names,
        global_cancellation.clone(),
    )?;

    ctrl_c().await.unwrap();
    global_cancellation.cancel();

    Ok(())
}
