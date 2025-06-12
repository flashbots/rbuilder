use crate::{get_timestamp_f64, types::BlockBid, DynResult, RPC_TIMEOUT};
use clap::Parser;
use ethers::prelude::*;
use futures_util::SinkExt;
use runng::{protocol::Pub0, Listen, SendSocket};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, protocol::Message};
use tracing::{debug, error, info};

#[derive(Parser, Clone)]
pub struct Args {
    #[clap(long)]
    pub bloxroute_url: String,
    #[clap(long)]
    pub auth_header: String,
    #[clap(long)]
    pub publisher_url: String,
    #[clap(
        long,
        help = "Path to the relays file. For sanity checking the relay_name is in there."
    )]
    pub relays_file: String,
    #[clap(long)]
    pub publisher_name: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct BloxrouteWsBid {
    relay_type: String,
    builder_pubkey: String,
    parent_hash: H256,
    block_hash: H256,
    timestamp_ms: u64,
    block_value: u128,
    slot_number: u64,
    #[serde(default)]
    gas_used: u64,
    proposer_fee_recipient: Address,
    optimistic_submission: Option<bool>,
}

#[derive(Clone)]
pub struct Service {
    args: Args,
    nng_publisher_socket: Pub0,
    relay_names: HashSet<String>,
}

impl Service {
    pub async fn new(args: Args) -> Self {
        let relays_file =
            std::fs::File::open(args.relays_file.clone()).expect("file should open read only");
        let relay_urls: HashMap<String, String> =
            serde_json::from_reader(relays_file).expect("file should be proper JSON");
        let relay_names: HashSet<String> = HashSet::from_iter(relay_urls.into_keys());

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
            relay_names,
        }
    }

    fn parse_bid(&self, json_bid: &serde_json::Value) -> DynResult<Option<BlockBid>> {
        let parsed = match serde_json::from_value::<BloxrouteWsBid>(json_bid.clone()) {
            Ok(bid) => bid,
            Err(error) => {
                error!(%error, json = %json_bid, "Error decoding bid");
                return Ok(None);
            }
        };

        let relay_name = format!("bloxroute-{}", parsed.relay_type);
        if !self.relay_names.contains(&relay_name) {
            return Err(format!(
                "Relay name {} should be in our list of relays: {:?}",
                &relay_name, &self.relay_names
            )
            .into());
        }

        let bid = BlockBid {
            publisher_name: self.args.publisher_name.clone(),
            publisher_type: "bloxroute_ws".to_owned(),
            builder_pubkey: Some(parsed.builder_pubkey.to_lowercase()),
            relay_name,
            parent_hash: parsed.parent_hash,
            block_hash: parsed.block_hash,
            seen_time: get_timestamp_f64(),
            relay_time: Some(parsed.timestamp_ms as f64 / 1000.),
            value: U256::from(parsed.block_value),
            slot_number: U64::from(parsed.slot_number),
            gas_used: Some(U64::from(parsed.gas_used)),
            proposer_fee_recipient: Some(parsed.proposer_fee_recipient),
            fee_recipient: None,
            optimistic_submission: parsed.optimistic_submission,
            block_number: None,
            extra_data: None,
        };
        debug!("Found bid: {bid:?}");

        Ok(Some(bid))
    }

    pub async fn run(self) {
        let mut request = self
            .args
            .bloxroute_url
            .clone()
            .into_client_request()
            .unwrap();
        let headers = request.headers_mut();
        headers.insert("Authorization", self.args.auth_header.parse().unwrap());
        let (ws_stream, _) = timeout(RPC_TIMEOUT, tokio_tungstenite::connect_async(request))
            .await
            .expect("timeout when connecting to bloxroute")
            .expect("unable to connect to bloxroute");

        let (mut write, mut read) = ws_stream.split();

        write
            .send(tokio_tungstenite::tungstenite::protocol::Message::Text(
                serde_json::to_string(&json!(
                    {
                        "id": 1,
                        "method": "subscribe",
                        "params": ["MEVBlockValue", {"include": []}],
                    }
                ))
                .unwrap(),
            ))
            .await
            .expect("unable to send first message");

        info!(
            "Got first message: {:?}",
            timeout(RPC_TIMEOUT, read.next())
                .await
                .expect("reading first message timed out")
                .expect("can't read first message")
        );

        info!("All ready, will publish bloxroute bids.");

        loop {
            let message = timeout(RPC_TIMEOUT, read.next())
                .await
                .expect("reading message timed out")
                .expect("can't read message")
                .expect("can't parse message");

            match message {
                Message::Text(data) => {
                    let json_bid: serde_json::Value =
                        serde_json::from_str(&data).expect("unable to parse message as json");

                    debug!("Got message: {}", json_bid);

                    assert!(!json_bid["params"]["subscription"].is_null());

                    if let Some(bid) = self
                        .parse_bid(&json_bid["params"]["result"])
                        .expect("unable to parse bid")
                    {
                        self.nng_publisher_socket
                            .send(&serde_json::to_vec(&bid).unwrap())
                            .expect("unable to send message");
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ws_bid() {
        // normal bid
        let raw = r#"{
            "blockHash": "0x79d966e4620001684016497f0d0d0938ec3bc72f98e07005b186e112f6401edb",
            "blockNumber": 19674664,
            "blockValue": 15071706065904500_u128,
            "builderPubkey": "0x95c8cc31f8d4e54eddb0603b8f12d59d466f656f374bde2073e321bdd16082d420e3eef4d62467a7ea6b83818381f742",
            "gasUsed": 13741841,
            "parentHash": "0x970aaa4627296e44da4db0a055242b79e4c1aedd98fa63e884e6b8f09cfaf08c",
            "proposerFeeRecipient": "0x22eec85ba6a5cd97ead4728ea1c69e1d9c6fa778",
            "relayType": "max-profit",
            "slotNumber": 8877207,
            "timestampMs": 1713350506069
        "#;
        assert!(serde_json::from_str::<BloxrouteWsBid>(raw).is_ok());

        // without gas used
        let raw = r#"{
            "blockHash": "0x5b4e03082f3c6ae3c4e8da16e698b10cd60352cd422f9655cfcf95d43fd231b6",
            "blockNumber": 19674664,
            "blockValue": 0,
            "builderPubkey": "0xb7e71108b8b1d03e79f2e9a9204bb007ebd62e32d609a7825eef7a98d10e95a87169cbf62bd240a53d7023470cbdbbb8",
            "parentHash": "0x970aaa4627296e44da4db0a055242b79e4c1aedd98fa63e884e6b8f09cfaf08c",
            "proposerFeeRecipient": "0x22eec85ba6a5cd97ead4728ea1c69e1d9c6fa778",
            "relayType": "max-profit",
            "slotNumber": 8877207,
            "timestampMs": 1713350506085
        "#;
        assert!(serde_json::from_str::<BloxrouteWsBid>(raw).is_ok());
    }
}
