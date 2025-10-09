use alloy_primitives::{BlockHash, U256};
use rbuilder_config::EnvOrValue;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::net::TcpStream;
use tokio_stream::StreamExt;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        client::IntoClientRequest, handshake::client::Request, protocol::Message, Error,
    },
    MaybeTlsStream, WebSocketStream,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::reconnect::{run_async_loop_with_reconnect, RunCommand};

type Connection = WebSocketStream<MaybeTlsStream<TcpStream>>;

const MAX_IO_ERRORS: usize = 5;

// time that we wait for a new value before reconnecting
const READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ExternalWsPublisherConfig {
    pub url: String,
    pub auth_header: EnvOrValue<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BestBidValue {
    pub block_number: u64,
    pub slot_number: u64,
    pub block_top_bid: U256,
    pub parent_hash: BlockHash,
}

pub trait BestBidValueSink {
    fn send(&self, bid: BestBidValue);
}

/// Struct that connects to a websocket feed with best bids from the competition.
/// Allows to subscribe so listen for changes on a particular slot.
/// Usage:
/// - call sub = subscribe
///     - monitor the value as long as needed:
///         - await wait_for_change. This will wake when a change happens, no need for polling.
///         - Ask top_bid
/// - call unsubscribe(sub)
#[derive(Debug)]
pub struct BestBidWSConnector<BestBidValueSinkType> {
    connection_request: Request,
    sink: Arc<BestBidValueSinkType>,
}

impl<BestBidValueSinkType: BestBidValueSink> BestBidWSConnector<BestBidValueSinkType> {
    pub fn new(url: &str, basic_auth: &str, sink: BestBidValueSinkType) -> eyre::Result<Self> {
        let mut connection_request = url.into_client_request()?;
        connection_request
            .headers_mut()
            .insert("Authorization", format!("Basic {basic_auth}").parse()?);

        Ok(Self {
            connection_request,
            sink: Arc::new(sink),
        })
    }

    pub async fn run_ws_stream(
        &self,
        // We must try_send on every non 0 bid or the process will be killed
        cancellation_token: CancellationToken,
    ) {
        run_async_loop_with_reconnect(
            "ws_top_bid_connection",
            || connect(self.connection_request.clone()),
            |conn| run_command(conn, self.sink.clone(), cancellation_token.clone()),
            None,
            cancellation_token.clone(),
        )
        .await;
    }
}

async fn connect<R>(request: R) -> Result<Connection, Error>
where
    R: IntoClientRequest + Unpin,
{
    connect_async_with_config(
        request, None, true, // TODO: naggle, decide
    )
    .await
    .map(|(c, _)| c)
}

async fn run_command<BestBidValueSinkType: BestBidValueSink>(
    mut conn: Connection,
    sink: Arc<BestBidValueSinkType>,
    cancellation_token: CancellationToken,
) -> RunCommand {
    let mut io_error_count = 0;
    loop {
        if cancellation_token.is_cancelled() {
            break;
        }
        if io_error_count >= MAX_IO_ERRORS {
            warn!("Too many read errors, reconnecting");
            return RunCommand::Reconnect;
        }

        let next_message = tokio::time::timeout(READ_TIMEOUT, conn.next());
        let res = match next_message.await {
            Ok(res) => res,
            Err(err) => {
                warn!(?err, "Timeout error");
                return RunCommand::Reconnect;
            }
        };
        let message = match res {
            Some(Ok(message)) => message,
            Some(Err(err)) => {
                warn!(?err, "Error reading WS stream");
                io_error_count += 1;
                continue;
            }
            None => {
                warn!("Connection read stream is closed, reconnecting");
                return RunCommand::Reconnect;
            }
        };
        let data = match &message {
            Message::Text(msg) => msg.as_bytes(),
            Message::Binary(msg) => msg.as_ref(),
            Message::Ping(_) => {
                error!(ws_message = "ping", "Received unexpected message");
                continue;
            }
            Message::Pong(_) => {
                error!(ws_message = "pong", "Received unexpected message");
                continue;
            }
            Message::Frame(_) => {
                error!(ws_message = "frame", "Received unexpected message");
                continue;
            }
            Message::Close(_) => {
                warn!("Connection closed, reconnecting");
                return RunCommand::Reconnect;
            }
        };

        let bid_value: BestBidValue = match serde_json::from_slice(data) {
            Ok(value) => value,
            Err(err) => {
                error!(?err, "Failed to parse best bid value");
                continue;
            }
        };
        sink.send(bid_value);
    }
    RunCommand::Finish
}
