use std::sync::Arc;

use crate::{bid_sender::BidSender, types::ScrapedRelayBlockBid, RPC_TIMEOUT};
use eyre::{eyre, Context};
use futures::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, http::Request, protocol::Message},
    MaybeTlsStream, WebSocketStream,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

/// Trait responsible for the specific WS connection
pub trait ConnectionHandler {
    fn url(&self) -> String;
    /// Add any headers you need
    fn configure_request(&self, request: &mut Request<()>) -> eyre::Result<()>;
    /// Send any initial packet
    #[allow(async_fn_in_trait)]
    async fn init_connection(
        &self,
        write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        read: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    ) -> eyre::Result<()>;
    /// No need to handle ping/pong. Only any accepted data.
    fn parse(&self, message: Message) -> eyre::Result<Option<ScrapedRelayBlockBid>>;
}
pub struct Service<ConnectionHandlerType: 'static> {
    handler: ConnectionHandlerType,
    sender: Arc<dyn BidSender>,
    cancel: CancellationToken,
}

impl<ConnectionHandlerType> Service<ConnectionHandlerType>
where
    ConnectionHandlerType: ConnectionHandler + 'static,
{
    pub async fn new(
        handler: ConnectionHandlerType,
        sender: Arc<dyn BidSender>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            handler,
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
            .handler
            .url()
            .into_client_request()
            .wrap_err("Unable to create request")?;
        self.handler.configure_request(&mut request)?;
        let (ws_stream, _) = timeout(RPC_TIMEOUT, tokio_tungstenite::connect_async(request))
            .await
            .wrap_err("timeout when connecting to ultrasound")?
            .wrap_err("unable to connect to ultrasound")?;

        let (mut write, mut read) = ws_stream.split();
        self.handler.init_connection(&mut write, &mut read).await?;

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
                msg => {
                    if let Some(bid) = self.handler.parse(msg)? {
                        debug!("Found bid: {bid:?}");
                        let _ = self.sender.send(bid);
                    }
                }
            }
        }
    }
}
