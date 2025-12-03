use crate::{
    get_timestamp_f64,
    types::{PublisherType, ScrapedRelayBlockBid},
    ws_publisher::{ConnectionHandler, Service},
    DynResult, RPC_TIMEOUT,
};
use alloy_primitives::{Address, BlockHash, U256};
use alloy_rpc_types_beacon::BlsPublicKey;
use futures::{
    stream::{SplitSink, SplitStream},
    StreamExt,
};
use futures_util::SinkExt;
use rbuilder_config::EnvOrValue;
use serde::Deserialize;
use serde_json::json;
use std::str::FromStr;
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{
    tungstenite::{http::Request, protocol::Message},
    MaybeTlsStream, WebSocketStream,
};
use tracing::{debug, error, info};

pub type BloxrouteWsPublisher = Service<BloxrouteWsConnectionHandler>;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct BloxrouteWsPublisherConfig {
    /// Url to connect to. Example: "wss://mev-eth.blxrbdn.com/ws"
    pub bloxroute_url: String,
    /// Be sure to use unique names. Maybe we can take it from the bloxroute_url?
    pub relay_name: String,
    /// Added as "Authorization" header.
    pub auth_header: EnvOrValue<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct BloxrouteWsBid {
    relay_type: String,
    builder_pubkey: String,
    parent_hash: BlockHash,
    block_hash: BlockHash,
    timestamp_ms: u64,
    block_value: u128,
    block_number: u64,
    slot_number: u64,
    #[serde(default)]
    gas_used: u64,
    proposer_fee_recipient: Address,
    optimistic_submission: Option<bool>,
}

pub struct BloxrouteWsConnectionHandler {
    cfg: BloxrouteWsPublisherConfig,
    name: String,
}

impl BloxrouteWsConnectionHandler {
    pub fn new(cfg: BloxrouteWsPublisherConfig, name: String) -> Self {
        Self { cfg, name }
    }

    fn parse_bid(&self, json_bid: &serde_json::Value) -> DynResult<Option<ScrapedRelayBlockBid>> {
        let parsed = match serde_json::from_value::<BloxrouteWsBid>(json_bid.clone()) {
            Ok(bid) => bid,
            Err(error) => {
                error!(%error, json = %json_bid, "Error decoding bid");
                return Ok(None);
            }
        };

        let relay_name = format!("bloxroute-{}", parsed.relay_type);

        let bid = ScrapedRelayBlockBid {
            publisher_name: self.name.clone(),
            publisher_type: PublisherType::BloxrouteWs,
            builder_pubkey: Some(BlsPublicKey::from_str(&parsed.builder_pubkey)?),
            relay_name,
            parent_hash: parsed.parent_hash,
            block_hash: parsed.block_hash,
            seen_time: get_timestamp_f64(),
            relay_time: Some(parsed.timestamp_ms as f64 / 1000.),
            value: U256::from(parsed.block_value),
            slot_number: parsed.slot_number,
            gas_used: Some(parsed.gas_used),
            proposer_fee_recipient: Some(parsed.proposer_fee_recipient),
            fee_recipient: None,
            optimistic_submission: parsed.optimistic_submission,
            block_number: parsed.block_number,
            extra_data: None,
        };
        debug!("Found bid: {bid:?}");

        Ok(Some(bid))
    }
}

impl ConnectionHandler for BloxrouteWsConnectionHandler {
    fn url(&self) -> String {
        self.cfg.bloxroute_url.clone()
    }

    fn configure_request(&self, request: &mut Request<()>) -> eyre::Result<()> {
        let headers = request.headers_mut();
        headers.insert("Authorization", self.cfg.auth_header.value()?.parse()?);
        Ok(())
    }

    async fn init_connection(
        &self,
        write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        read: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    ) -> eyre::Result<()> {
        write
            .send(tokio_tungstenite::tungstenite::protocol::Message::Text(
                serde_json::to_string(&json!(
                    {
                        "id": 1,
                        "method": "subscribe",
                        "params": ["MEVBlockValue", {"include": []}],
                    }
                ))?
                .into(),
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
        Ok(())
    }

    fn parse(&self, message: Message) -> eyre::Result<Option<ScrapedRelayBlockBid>> {
        match message {
            Message::Text(data) => {
                let json_bid: serde_json::Value =
                    serde_json::from_str(&data).expect("unable to parse message as json");
                debug!("Got message: {}", json_bid);

                assert!(!json_bid["params"]["subscription"].is_null());

                Ok(self
                    .parse_bid(&json_bid["params"]["result"])
                    .expect("unable to parse bid"))
            }
            _ => {
                eyre::bail!("Unhandled bloxroute WS message: {:?}", message);
            }
        }
    }
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
            "blockValue": 15071706065904500,
            "builderPubkey": "0x95c8cc31f8d4e54eddb0603b8f12d59d466f656f374bde2073e321bdd16082d420e3eef4d62467a7ea6b83818381f742",
            "gasUsed": 13741841,
            "parentHash": "0x970aaa4627296e44da4db0a055242b79e4c1aedd98fa63e884e6b8f09cfaf08c",
            "proposerFeeRecipient": "0x22eec85ba6a5cd97ead4728ea1c69e1d9c6fa778",
            "relayType": "max-profit",
            "slotNumber": 8877207,
            "timestampMs": 1713350506069
        }"#;
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
        }"#;
        assert!(serde_json::from_str::<BloxrouteWsBid>(raw).is_ok());
    }
}
