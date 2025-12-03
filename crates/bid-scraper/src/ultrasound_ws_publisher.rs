use crate::{
    types::{block_bid_from_update, PublisherType, ScrapedRelayBlockBid, TopBidUpdate},
    ws_publisher::{ConnectionHandler, Service},
};
use eyre::{eyre, Context};
use futures::stream::{SplitSink, SplitStream};
use serde::Deserialize;
use ssz::Decode;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    tungstenite::{http::Request, protocol::Message},
    MaybeTlsStream, WebSocketStream,
};
use tracing::debug;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct UltrasoundWsPublisherConfig {
    /// Url to connect to. Example: "ws://relay-builders-eu.ultrasound.money/ws/v1/top_bid"
    pub ultrasound_url: String,
    /// Be sure to use unique names. Maybe we can take it from the ultrasound_url?
    pub relay_name: String,
    /// Used as header X-Builder-Id, for use with ultrasound builder direct endpoint
    pub builder_id: Option<String>,
    /// used as header X-Api-Token, for use with ultrasound builder direct endpoint
    pub api_token: Option<String>,
}

pub struct UltrasoundWsConnectionHandler {
    cfg: UltrasoundWsPublisherConfig,
    name: String,
}

impl UltrasoundWsConnectionHandler {
    pub fn new(cfg: UltrasoundWsPublisherConfig, name: String) -> Self {
        Self { cfg, name }
    }
}

impl ConnectionHandler for UltrasoundWsConnectionHandler {
    fn url(&self) -> String {
        self.cfg.ultrasound_url.clone()
    }
    fn configure_request(&self, request: &mut Request<()>) -> eyre::Result<()> {
        if let (Some(builder_id), Some(api_token)) = (&self.cfg.builder_id, &self.cfg.api_token) {
            let headers = request.headers_mut();
            let builder_id_header_value =
                tokio_tungstenite::tungstenite::http::HeaderValue::from_str(builder_id)
                    .wrap_err("Invalid header value for 'X-Builder-Id'")?;
            headers.insert("X-Builder-Id", builder_id_header_value);
            let api_token_header_value =
                tokio_tungstenite::tungstenite::http::HeaderValue::from_str(api_token)
                    .wrap_err("Invalid header value for 'X-Api-Token'")?;
            headers.insert("X-Api-Token", api_token_header_value);
        }
        Ok(())
    }

    async fn init_connection(
        &self,
        _write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        _read: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    ) -> eyre::Result<()> {
        Ok(())
    }

    fn parse(&self, message: Message) -> eyre::Result<Option<ScrapedRelayBlockBid>> {
        match message {
            Message::Binary(data) => {
                let update = TopBidUpdate::from_ssz_bytes(&data)
                    .map_err(|_| eyre!("unable to deserialize"))?;
                debug!("Got message: {:?}", update);
                let bid = block_bid_from_update(
                    update,
                    &self.cfg.relay_name,
                    &self.name,
                    PublisherType::UltrasoundWs,
                );
                Ok(Some(bid))
            }
            _ => {
                eyre::bail!("Unhandled ultrasound WS message: {:?}", message);
            }
        }
    }
}

pub type UltrasoundWsPublisher = Service<UltrasoundWsConnectionHandler>;
