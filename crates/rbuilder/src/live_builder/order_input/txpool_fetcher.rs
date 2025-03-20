use super::{OrderInputConfig, ReplaceableOrderPoolCommand};
use crate::{
    primitives::{MempoolTx, Order, TransactionSignedEcRecoveredWithBlobs},
    telemetry::add_txfetcher_time_to_query,
};
use alloy_primitives::{hex, Bytes};
use ethers::{
    middleware::Middleware,
    providers::{Ipc, Provider},
};
use futures::StreamExt;
use primitive_types::H256;
use std::{pin::pin, time::Instant};
use tokio::{
    sync::{mpsc, mpsc::error::SendTimeoutError},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace};

/// Subscribes to EL mempool and pushes new txs as orders in results.
/// This version allows 4844 by subscribing to subscribe_pending_txs to get the hashes and then calling eth_getRawTransactionByHash
/// to get the raw tx that, in case of 4844 tx, may include blobs.
/// In the future we may consider updating reth so we can process blob txs in a different task to avoid slowing down non blob txs.
pub async fn subscribe_to_txpool_with_blobs(
    config: OrderInputConfig,
    results: mpsc::Sender<ReplaceableOrderPoolCommand>,
    global_cancel: CancellationToken,
) -> eyre::Result<JoinHandle<()>> {
    let ipc = Ipc::connect(config.ipc_path).await?;

    let handle = tokio::spawn(async move {
        info!("Subscribe to txpool with blobs: started");

        let provider = Provider::new(ipc);
        let stream = match provider.subscribe_pending_txs().await {
            Ok(stream) => stream,
            Err(err) => {
                error!(?err, "Failed to subscribe to ipc txpool stream");
                // Closing builder because this job is critical so maybe restart will help
                global_cancel.cancel();
                return;
            }
        };
        let mut stream = pin!(stream.take_until(global_cancel.cancelled()));

        loop {
            let tx_hash = match stream.next().await {
                Some(tx_hash) => tx_hash,
                None => {
                    // stream is closed, cancelling token because builder can't work without this stream
                    global_cancel.cancel();
                    break;
                }
            };
            let start = Instant::now();

            let tx_with_blobs = match get_tx_with_blobs(tx_hash, &provider).await {
                Ok(Some(tx_with_blobs)) => tx_with_blobs,
                Ok(None) => {
                    trace!(?tx_hash, "tx not found in tx pool");
                    continue;
                }
                Err(err) => {
                    error!(?tx_hash, ?err, "Failed to get tx pool");
                    continue;
                }
            };
            let tx = MempoolTx::new(tx_with_blobs);
            let order = Order::Tx(tx);
            let parse_duration = start.elapsed();
            trace!(order = ?order.id(), parse_duration_mus = parse_duration.as_micros(), "Mempool transaction received with blobs");
            add_txfetcher_time_to_query(parse_duration);
            // debug!("received mempool tx (id={}).", order.id());
            match results
                .send_timeout(
                    ReplaceableOrderPoolCommand::Order(order),
                    config.results_channel_timeout,
                )
                .await
            {
                Ok(()) => {}
                Err(SendTimeoutError::Timeout(_)) => {
                    error!("Failed to send txpool tx to results channel, timeout");
                }
                Err(SendTimeoutError::Closed(_)) => {
                    break;
                }
            }
        }

        global_cancel.cancel();
        info!("Subscribe to txpool: finished");
    });

    Ok(handle)
}

async fn get_tx_with_blobs(
    tx_hash: H256,
    provider: &Provider<Ipc>,
) -> eyre::Result<Option<TransactionSignedEcRecoveredWithBlobs>> {
    let raw_tx: Option<String> = provider
        .request("eth_getRawTransactionByHash", vec![tx_hash])
        .await?;

    let raw_tx = if let Some(raw_tx) = raw_tx {
        raw_tx
    } else {
        return Ok(None);
    };

    let raw_tx = hex::decode(raw_tx)?;
    let raw_tx = Bytes::from(raw_tx);
    Ok(Some(
        TransactionSignedEcRecoveredWithBlobs::decode_enveloped_with_real_blobs(raw_tx)?,
    ))
}
