use crate::{
    types::{block_bid_from_update, TopBidUpdate},
    RPC_TIMEOUT,
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use runng::{protocol::Pub0, Listen, SendSocket};
use ssz::Decode;
use std::collections::HashMap;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, protocol::Message};
use tracing::{debug, info};

#[derive(Parser, Clone)]
pub struct Args {
    #[clap(long)]
    pub ultrasound_url: String,
    #[clap(long)]
    pub publisher_url: String,
    #[clap(
        long,
        help = "Path to the relays file. For sanity checking the relay_name is in there."
    )]
    pub relays_file: String,
    #[clap(long)]
    pub relay_name: String,
    #[clap(long)]
    pub publisher_name: String,
    #[clap(
        long,
        requires = "api-token",
        help = "used as header X-Builder-Id, for use with ultrasound builder direct endpoint"
    )]
    pub builder_id: Option<String>,
    #[clap(
        long,
        requires = "builder-id",
        help = "used as header X-Api-Token, for use with ultrasound builder direct endpoint"
    )]
    pub api_token: Option<String>,
}

#[derive(Clone)]
pub struct Service {
    args: Args,
    nng_publisher_socket: Pub0,
}

impl Service {
    pub async fn new(args: Args) -> Self {
        let relays_file =
            std::fs::File::open(args.relays_file.clone()).expect("file should open read only");
        let relay_urls: HashMap<String, String> =
            serde_json::from_reader(relays_file).expect("file should be proper JSON");
        assert!(
            relay_urls.contains_key(&args.relay_name),
            "Specified relay name {} should be in our list of relays: {:?}",
            &args.relay_name,
            &relay_urls.keys()
        );

        let runng_factory = runng::factory::latest::ProtocolFactory::default();
        let mut nng_publisher_socket = runng_factory
            .publisher_open()
            .expect("unable to create NNG publisher");
        nng_publisher_socket
            .listen(&args.publisher_url)
            .expect("unable to have the NNG publisher listen");

        Self {
            args,
            nng_publisher_socket,
        }
    }

    pub async fn run(self) {
        let mut request = self
            .args
            .ultrasound_url
            .clone()
            .into_client_request()
            .unwrap();
        if let (Some(builder_id), Some(api_token)) = (&self.args.builder_id, &self.args.api_token) {
            let headers = request.headers_mut();
            let builder_id_header_value = reqwest::header::HeaderValue::from_str(builder_id)
                .expect("Invalid header value for 'X-Builder-Id'");
            headers.insert("X-Builder-Id", builder_id_header_value);
            let api_token_header_value = reqwest::header::HeaderValue::from_str(api_token)
                .expect("Invalid header value for 'X-Api-Token'");
            headers.insert("X-Api-Token", api_token_header_value);
        }
        let (ws_stream, _) = timeout(RPC_TIMEOUT, tokio_tungstenite::connect_async(request))
            .await
            .expect("timeout when connecting to ultrasound")
            .expect("unable to connect to ultrasound");

        let (mut write, mut read) = ws_stream.split();

        info!("All ready, listening to bids.");

        loop {
            let message = timeout(RPC_TIMEOUT, read.next())
                .await
                .expect("reading message timed out")
                .expect("can't read message")
                .expect("can't parse message");

            match message {
                Message::Binary(data) => {
                    let update =
                        TopBidUpdate::from_ssz_bytes(&data).expect("unable to deserialize");

                    debug!("Got message: {:?}", update);

                    let bid = block_bid_from_update(
                        update,
                        &self.args.relay_name,
                        &self.args.publisher_name,
                        "ultrasound_ws",
                    );
                    debug!("Found bid: {bid:?}");

                    self.nng_publisher_socket
                        .send(&serde_json::to_vec(&bid).unwrap())
                        .expect("unable to send message");
                }
                Message::Ping(data) => {
                    info!("Got ping (size {}), sending pong.", data.len());
                    timeout(RPC_TIMEOUT, write.send(Message::Pong(data)))
                        .await
                        .expect("timeout while sending pong")
                        .expect("unable to send pong");
                }
                Message::Pong(data) => {
                    info!("Got pong (size {}).", data.len());
                }
                _ => {
                    panic!("Unhandled WS message: {:?}", message);
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // when one task crashes we crash the whole program
    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        orig_hook(panic_info);
        std::process::exit(1);
    }));

    let args = Args::parse();

    info!("Initializing service...");
    let service = Service::new(args.clone()).await;
    info!("Service initialized!");

    service.run().await;
}
