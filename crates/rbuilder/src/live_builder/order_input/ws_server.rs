use crate::live_builder::order_input::rpc_server::send_order;
use crate::live_builder::order_input::{OrderInputConfig, ReplaceableOrderPoolCommand};
use crate::primitives::serialize::{RawTx, TxEncoding};
use crate::primitives::{MempoolTx, Order};
use crate::telemetry::mark_command_received;
use crate::toba::BidAction;
use alloy_primitives::Bytes;
use futures::StreamExt;
use jsonrpsee::types::ErrorObject;
use std::net::{SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::SendTimeoutError;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::{Error, Message};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace, warn};

pub async fn start_toba_ws_server(
    config: OrderInputConfig,
    results: mpsc::Sender<ReplaceableOrderPoolCommand>,
    global_cancel: CancellationToken,
) -> eyre::Result<JoinHandle<()>> {
    let addr = SocketAddr::V4(SocketAddrV4::new(
        config.server_ip,
        /* config.toba_ws_port */ 3030,
    ));
    let listener = TcpListener::bind(addr).await?;
    let timeout = config.results_channel_timeout;

    async fn handle_connection(
        stream: tokio::net::TcpStream,
        timeout: Duration,
        results: mpsc::Sender<ReplaceableOrderPoolCommand>,
    ) {
        let ws_stream = tokio_tungstenite::accept_async(stream)
            .await
            .expect("Error accepting WebSocket connection");

        let (_, read) = ws_stream.split();
        read.for_each(|message| async {
            match message {
                Ok(Message::Text(text)) => {
                    let action = match serde_json::from_str::<BidAction>(&text){
                        Ok(action) => action,
                        Err(e) => {
                            warn!(error = ?e, "Failed to parse BidAction");
                            return;
                        }
                    };
                    match action {
                        BidAction::SubmitBid { transaction } => {
                            let received_at = OffsetDateTime::now_utc();
                            let start = Instant::now();
                            if let Ok(bytes) = (&transaction).parse() {
                                let tx: MempoolTx = match TxEncoding::WithBlobData.decode(bytes) {
                                    Ok(tx) => MempoolTx::new(tx),
                                    Err(err) => {
                                        warn!(?err, "Failed to decode raw transaction");
                                        return;
                                    }
                                };
                                let order = Order::Tx(tx);
                                let parse_duration = start.elapsed();
                                trace!(order = ?order.id(), parse_duration_mus = parse_duration.as_micros(), "Received TOBA tx from WebSocket");
                                send_order(order, &results, timeout, received_at, None).await;
                            } else {
                                // Handle the case where parsing fails
                                warn!("Failed to parse transaction");
                            }

                        }
                        BidAction::CancelBid { .. } => { todo!("Cancel bid is not supported yet") }
                    }
                },
                Ok(_) => {}
                Err(e) => { error!("Error processing WebSocket message: {}", e);}
            };
        })
            .await;
    }

    Ok(tokio::spawn(async move {
        info!("TOBA WebSocket listening on: ws://{}", addr);
        tokio::select! {
            _ = global_cancel.cancelled() => {},
            Ok((stream, addr)) = listener.accept() => {
                info!("New connection from: {}", addr);
                tokio::spawn(handle_connection(stream, timeout, results.clone()));
            },
        }
        info!("TOBA WebSocket server: finished");
    }))
}
