use crate::{
    bid_sender::BidSender,
    types::{block_bid_from_update, PublisherType, TopBidUpdate},
    RPC_TIMEOUT,
};
use eyre::{eyre, Context};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use ssz::Decode;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, protocol::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

#[derive(Debug, Clone, Deserialize)]
pub struct UltrasoundWsPublisherConfig {
    pub ultrasound_url: String,
    /// Be sure to use unique names. Maybe we can take it from the ultrasound_url?
    pub relay_name: String,
    /// Used as header X-Builder-Id, for use with ultrasound builder direct endpoint
    pub builder_id: Option<String>,
    /// used as header X-Api-Token, for use with ultrasound builder direct endpoint
    pub api_token: Option<String>,
}

pub struct Service {
    cfg: UltrasoundWsPublisherConfig,
    name: String,
    sender: BidSender,
    cancel: CancellationToken,
}

impl Service {
    pub async fn new(
        cfg: UltrasoundWsPublisherConfig,
        name: String,
        sender: BidSender,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            cfg,
            name,
            sender,
            cancel,
        }
    }

    pub async fn run(self) {
        if let Err(err) = self.run_with_error().await {
            error!(err=?err, "UltrasoundWs failed");
        }
    }

    async fn run_with_error(self) -> eyre::Result<()> {
        let mut request = self
            .cfg
            .ultrasound_url
            .clone()
            .into_client_request()
            .wrap_err("Unable to create request")?;
        if let (Some(builder_id), Some(api_token)) = (&self.cfg.builder_id, &self.cfg.api_token) {
            let headers = request.headers_mut();
            let builder_id_header_value = reqwest::header::HeaderValue::from_str(builder_id)
                .wrap_err("Invalid header value for 'X-Builder-Id'")?;
            headers.insert("X-Builder-Id", builder_id_header_value);
            let api_token_header_value = reqwest::header::HeaderValue::from_str(api_token)
                .wrap_err("Invalid header value for 'X-Api-Token'")?;
            headers.insert("X-Api-Token", api_token_header_value);
        }
        let (ws_stream, _) = timeout(RPC_TIMEOUT, tokio_tungstenite::connect_async(request))
            .await
            .wrap_err("timeout when connecting to ultrasound")?
            .wrap_err("unable to connect to ultrasound")?;

        let (mut write, mut read) = ws_stream.split();

        info!("All ready, listening to bids.");
        loop {
            let message = tokio::select! {
                message = timeout(RPC_TIMEOUT, read.next()) => {
                    message.wrap_err( "reading message timed out")?
                    .ok_or(eyre!("can't read message"))?
                    .wrap_err( "can't parse message")?
                }
                _ = self.cancel.cancelled() =>{
                    return Ok(());
                }
            };
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
                    debug!("Found bid: {bid:?}");

                    let _ = self.sender.send(bid);
                }
                Message::Ping(data) => {
                    info!("Got ping (size {}), sending pong.", data.len());
                    timeout(RPC_TIMEOUT, write.send(Message::Pong(data)))
                        .await
                        .wrap_err("timeout while sending pong")?
                        .wrap_err("unable to send pong")?;
                }
                Message::Pong(data) => {
                    info!("Got pong (size {}).", data.len());
                }
                _ => {
                    eyre::bail!("Unhandled WS message: {:?}", message);
                }
            }
        }
    }
}
