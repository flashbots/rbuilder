use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use relay_bids_publisher::{
    types::{block_bid_from_update, TopBidUpdate},
    RPC_TIMEOUT,
};
use runng::{protocol::Pub0, Listen, SendSocket};
use ssz::Decode;
use std::collections::HashMap;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, protocol::Message};
use tracing::{debug, info};

#[derive(Parser, Clone)]
pub struct Args {
    #[clap(long)]
    pub titan_url: String,
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
    #[clap(long, help = "used as X-api-key header")]
    pub api_key: String,
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
        let mut request = self.args.titan_url.clone().into_client_request().unwrap();
        let headers = request.headers_mut();
        headers.insert(
            "X-api-key",
            reqwest::header::HeaderValue::from_str(&self.args.api_key)
                .expect("Invalid header value for 'X-Api-Token'"),
        );
        let (ws_stream, _) = timeout(RPC_TIMEOUT, tokio_tungstenite::connect_async(request))
            .await
            .expect("timeout when connecting to titan")
            .expect("unable to connect to titan");

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
                        "titan_ws",
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
