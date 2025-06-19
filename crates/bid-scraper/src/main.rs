use bid_scraper::bid_sender::BidSender;
use bid_scraper::bids_publisher::{BidsPublisherService, RelayBidsPublisherConfig};
use bid_scraper::bloxroute_ws_publisher::BloxrouteWsPublisherConfig;
use bid_scraper::code_from_rbuilder::{
    load_config_toml_and_env, setup_tracing_subscriber, LoggerConfig,
};
use bid_scraper::config::{Config, PublisherConfig};
use bid_scraper::headers_publisher::{HeadersPublisherService, RelayHeadersPublisherConfig};
use bid_scraper::relay_api_publisher::CfgWithSimpleRelayPublisherConfig;
use bid_scraper::ultrasound_ws_publisher::UltrasoundWsPublisherConfig;
use runng::protocol::Pub0;
use runng::Listen;
use std::env;
use std::time::Duration;
use tokio::signal::ctrl_c;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        println!("Man, it's not that hard. It's a single parameter: the config file name. Something like:\n{} /home/cool_user_name/some_dir_to_keep_things_nice/some_other_dir_since_im_OCD/another_one/why_are_you_stil_reading_this_question_mark/stop_reading_and_fix_your_command_line/config_file.toml",args[0]);
        return Ok(());
    }

    let config: Config = load_config_toml_and_env(args[1].clone())?;

    let log_config = LoggerConfig {
        env_filter: config.log_level.clone(),
        log_json: config.log_json,
        log_color: config.log_color,
    };
    setup_tracing_subscriber(log_config)?;

    let global_cancel = CancellationToken::new();
    let global_cancel_clone = global_cancel.clone();
    let ctrlc = tokio::spawn(async move {
        ctrl_c().await.unwrap_or_default();
        global_cancel_clone.cancel()
    });

    let runng_factory = runng::factory::latest::ProtocolFactory::default();
    let mut nng_publisher_socket = runng_factory
        .publisher_open()
        .expect("unable to create NNG publisher");
    nng_publisher_socket
        .listen(&config.publisher_url)
        .expect("unable to have the NNG publisher listen");

    println!("{:?}", config.clone());
    for named_publisher in config.publishers {
        match named_publisher.publisher {
            PublisherConfig::RelayBids(cfg) => {
                tokio::spawn(start_relay_publisher::<
                    RelayBidsPublisherConfig,
                    BidsPublisherService,
                >(
                    cfg,
                    named_publisher.name,
                    nng_publisher_socket.clone(),
                    global_cancel.clone(),
                ));
            }
            PublisherConfig::RelayHeaders(cfg) => {
                tokio::spawn(start_relay_publisher::<
                    RelayHeadersPublisherConfig,
                    HeadersPublisherService,
                >(
                    cfg,
                    named_publisher.name,
                    nng_publisher_socket.clone(),
                    global_cancel.clone(),
                ));
            }
            PublisherConfig::UltrasoundWs(cfg) => {
                tokio::spawn(start_ultrasound_publisher(
                    cfg,
                    named_publisher.name,
                    nng_publisher_socket.clone(),
                    global_cancel.clone(),
                ));
            }
            PublisherConfig::BloxrouteWs(cfg) => {
                tokio::spawn(start_bloxroute_publisher(
                    cfg,
                    named_publisher.name,
                    nng_publisher_socket.clone(),
                    global_cancel.clone(),
                ));
            }
        };
    }
    ctrlc.await.unwrap_or_default();
    Ok(())
}

async fn start_bloxroute_publisher(
    cfg: BloxrouteWsPublisherConfig,
    name: String,
    nng_publisher_socket: Pub0,
    global_cancel: CancellationToken,
) {
    while !global_cancel.is_cancelled() {
        info!(name, "Initializing service...");
        let session_cancel = global_cancel.child_token();
        let sender = BidSender::new(
            nng_publisher_socket.clone(),
            global_cancel.clone(),
            session_cancel.clone(),
        );

        let service = bid_scraper::bloxroute_ws_publisher::Service::new(
            cfg.clone(),
            name.clone(),
            sender,
            session_cancel,
        )
        .await;
        info!(name, "Service initialized!");
        service.run().await;

        info!(name, "Service died waiting to restart it");
        let _ = timeout(Duration::from_secs(10), global_cancel.cancelled()).await;
    }
}

async fn start_ultrasound_publisher(
    cfg: UltrasoundWsPublisherConfig,
    name: String,
    nng_publisher_socket: Pub0,
    global_cancel: CancellationToken,
) {
    while !global_cancel.is_cancelled() {
        info!(name, "Initializing service...");
        let session_cancel = global_cancel.child_token();
        let sender = BidSender::new(
            nng_publisher_socket.clone(),
            global_cancel.clone(),
            session_cancel.clone(),
        );

        let service = bid_scraper::ultrasound_ws_publisher::Service::new(
            cfg.clone(),
            name.clone(),
            sender,
            session_cancel,
        )
        .await;
        info!(name, "Service initialized!");
        service.run().await;

        info!(name, "Service died waiting to restart it");
        let _ = timeout(Duration::from_secs(10), global_cancel.cancelled()).await;
    }
}

async fn start_relay_publisher<
    CfgType: CfgWithSimpleRelayPublisherConfig + Clone,
    ServiceType: bid_scraper::relay_api_publisher::Service<CfgType> + Send + Sync + 'static,
>(
    cfg: CfgType,
    name: String,
    nng_publisher_socket: Pub0,
    global_cancel: CancellationToken,
) {
    while !global_cancel.is_cancelled() {
        info!(name, "Initializing service...");
        let session_cancel = global_cancel.child_token();
        let sender = BidSender::new(
            nng_publisher_socket.clone(),
            global_cancel.clone(),
            session_cancel.clone(),
        );
        let timeout_secs =
            match ServiceType::new(cfg.clone(), name.clone(), sender, session_cancel).await {
                Ok(service) => {
                    info!(name, "Service initialized!");
                    service.run().await;
                    info!(name, "Service died waiting to restart it");
                    10
                }
                Err(err) => {
                    error!(err=?err, name, "Unable to create publisher");
                    60
                }
            };
        let _ = timeout(Duration::from_secs(timeout_secs), global_cancel.cancelled()).await;
    }
}
