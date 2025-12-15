//! PUAJ: copy/paste from ultrasound_ws_publisher.rs
use crate::{
    types::{top_bid_update::parse_message, PublisherType, ScrapedRelayBlockBid},
    ws_publisher::{ConnectionHandler, Service},
};
use eyre::Context;
use futures::stream::{SplitSink, SplitStream};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    tungstenite::{http::Request, protocol::Message},
    MaybeTlsStream, WebSocketStream,
};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TitanWsPublisherConfig {
    /// Url to connect to. Example: "ws://us.titanrelay.xyz/builder/top_bid"
    pub titan_url: String,
    /// Be sure to use unique names. Maybe we can take it from the titan_url?
    pub relay_name: String,
    /// used as header X-Api-Token, for use with titan builder direct endpoint
    pub api_token: String,
}

pub struct TitanWsConnectionHandler {
    cfg: TitanWsPublisherConfig,
    name: String,
}

impl TitanWsConnectionHandler {
    pub fn new(cfg: TitanWsPublisherConfig, name: String) -> Self {
        Self { cfg, name }
    }
}

impl ConnectionHandler for TitanWsConnectionHandler {
    fn url(&self) -> String {
        self.cfg.titan_url.clone()
    }
    fn configure_request(&self, request: &mut Request<()>) -> eyre::Result<()> {
        let headers = request.headers_mut();
        let api_token_header_value =
            tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&self.cfg.api_token)
                .wrap_err("Invalid header value for 'X-Api-Token'")?;
        headers.insert("X-Api-Token", api_token_header_value);
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
        parse_message(
            message,
            &self.cfg.relay_name,
            &self.name,
            PublisherType::TitanWs,
        )
    }
}

pub type TitanWsPublisher = Service<TitanWsConnectionHandler>;
