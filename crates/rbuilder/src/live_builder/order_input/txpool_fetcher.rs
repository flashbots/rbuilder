use super::{MempoolSource, OrderInputConfig, ReplaceableOrderPoolCommand};
use crate::telemetry::{add_txfetcher_time_to_query, mark_command_received};
use alloy_primitives::FixedBytes;
use alloy_provider::{IpcConnect, Provider, ProviderBuilder};
use futures::StreamExt;
use rbuilder_primitives::{
    serialize::TxEncoding, MempoolTx, Order, RawTransactionDecodable,
    TransactionSignedEcRecoveredWithBlobs,
};
use std::{pin::pin, sync::Arc, time::Instant};
use time::OffsetDateTime;
use tokio::{
    sync::{mpsc, mpsc::error::SendTimeoutError, Semaphore},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace, warn};

/// Max concurrent tx fetch requests to prevent overwhelming the RPC connection.
const TX_FETCH_CONCURRENCY: usize = 64;

/// Subscribes to EL mempool and pushes new txs as orders in results.
/// This version allows 4844 by subscribing to subscribe_pending_txs to get the hashes and then calling eth_getRawTransactionByHash
/// to get the raw tx that, in case of 4844 tx, may include blobs.
///
/// Uses a separate IPC connection for RPC calls and fetches transactions
/// concurrently to avoid blocking the subscription stream.
pub async fn subscribe_to_txpool_with_blobs(
    config: OrderInputConfig,
    results: mpsc::Sender<ReplaceableOrderPoolCommand>,
    global_cancel: CancellationToken,
) -> eyre::Result<JoinHandle<()>> {
    let mempool = config
        .mempool_source
        .ok_or_else(|| eyre::eyre!("No txpool source configured"))?;

    // Create TWO connections: one for subscription stream, one for RPC calls.
    // This prevents sequential get_raw_transaction_by_hash calls from blocking
    // the subscription consumer and causing backpressure on the IPC socket.
    let (sub_provider, rpc_provider) = match &mempool {
        MempoolSource::Ipc(path) => {
            let ipc1 = IpcConnect::new(path.clone());
            let ipc2 = IpcConnect::new(path.clone());
            let p1 = ProviderBuilder::new().connect_ipc(ipc1).await?;
            let p2 = ProviderBuilder::new().connect_ipc(ipc2).await?;
            (p1, Arc::new(p2))
        }
        MempoolSource::Ws(url) => {
            let ws1 = alloy_provider::WsConnect::new(url.clone());
            let ws2 = alloy_provider::WsConnect::new(url.clone());
            let p1 = ProviderBuilder::new().connect_ws(ws1).await?;
            let p2 = ProviderBuilder::new().connect_ws(ws2).await?;
            (p1, Arc::new(p2))
        }
    };

    let handle = tokio::spawn(async move {
        info!("Subscribe to txpool with blobs: started");

        let stream = match sub_provider
            .subscribe_pending_transactions()
            .channel_size(16384)
            .await
        {
            Ok(stream) => stream.into_stream().take_until(global_cancel.cancelled()),
            Err(err) => {
                error!(?err, "Failed to subscribe to ipc txpool stream");
                global_cancel.cancel();
                return;
            }
        };
        let mut stream = pin!(stream);

        // Semaphore limits concurrent fetch tasks to avoid overwhelming the RPC connection
        let semaphore = Arc::new(Semaphore::new(TX_FETCH_CONCURRENCY));

        // Consume tx hash notifications as fast as possible, spawning concurrent fetch tasks
        while let Some(tx_hash) = stream.next().await {
            let permit = match semaphore.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    // All fetch slots busy — drop this hash rather than blocking the stream
                    trace!(?tx_hash, "tx fetch concurrency limit reached, dropping hash");
                    continue;
                }
            };

            let provider = rpc_provider.clone();
            let results = results.clone();
            let config_timeout = config.results_channel_timeout;

            tokio::spawn(async move {
                let _permit = permit; // held until task completes
                let received_at = OffsetDateTime::now_utc();
                let start = Instant::now();

                let tx_with_blobs = match get_tx_with_blobs(tx_hash, provider.as_ref()).await {
                    Ok(Some(tx)) => tx,
                    Ok(None) => {
                        trace!(?tx_hash, "tx not found in tx pool");
                        return;
                    }
                    Err(err) => {
                        trace!(?tx_hash, ?err, "Failed to get tx from pool");
                        return;
                    }
                };

                let tx = MempoolTx::new(tx_with_blobs);
                let order = Order::Tx(tx);
                let parse_duration = start.elapsed();
                trace!(order = ?order.id(), parse_duration_mus = parse_duration.as_micros(), "Mempool transaction received with blobs");
                add_txfetcher_time_to_query(parse_duration);

                let orderpool_command = ReplaceableOrderPoolCommand::Order(Arc::new(order));
                mark_command_received(&orderpool_command, received_at);
                match results
                    .send_timeout(orderpool_command, config_timeout)
                    .await
                {
                    Ok(()) => {}
                    Err(SendTimeoutError::Timeout(_)) => {
                        warn!("Failed to send txpool tx to results channel, timeout");
                    }
                    Err(SendTimeoutError::Closed(_)) => {}
                }
            });
        }

        // stream ended, cancelling token because builder can't work without this stream
        global_cancel.cancel();
        info!("Subscribe to txpool: finished");
    });

    Ok(handle)
}

/// Calls eth_getRawTransactionByHash on EL node and decodes.
async fn get_tx_with_blobs(
    tx_hash: FixedBytes<32>,
    provider: &impl alloy_provider::Provider,
) -> eyre::Result<Option<TransactionSignedEcRecoveredWithBlobs>> {
    let Some(response) = provider.get_raw_transaction_by_hash(tx_hash).await? else {
        return Ok(None);
    };
    let raw_decodable = RawTransactionDecodable::new(response, TxEncoding::WithBlobData);
    Ok(Some(raw_decodable.decode_enveloped()?))
}

#[cfg(test)]
mod test {

    use super::*;
    use alloy_consensus::{SidecarBuilder, SimpleCoder};
    use alloy_network::{EthereumWallet, TransactionBuilder};
    use alloy_node_bindings::Anvil;
    use alloy_primitives::U256;
    use alloy_provider::{Provider, ProviderBuilder};
    use alloy_rpc_types::TransactionRequest;
    use alloy_signer_local::PrivateKeySigner;
    use std::path::PathBuf;

    #[tokio::test]
    /// Test that the fetcher can retrieve transactions (both normal and blob) from the txpool
    async fn test_fetcher_retrieves_transactions() {
        let anvil = Anvil::new()
            .args(["--ipc", "/tmp/anvil.ipc"])
            .try_spawn()
            .unwrap();

        let (sender, mut receiver) = mpsc::channel(10);
        subscribe_to_txpool_with_blobs(
            OrderInputConfig {
                mempool_source: Some(MempoolSource::Ipc(PathBuf::from("/tmp/anvil.ipc"))),
                ..OrderInputConfig::default_e2e()
            },
            sender,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
        let wallet = EthereumWallet::from(signer);

        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .connect_http(anvil.endpoint().parse().unwrap());

        let alice = anvil.addresses()[0];

        let sidecar: SidecarBuilder<SimpleCoder> =
            SidecarBuilder::from_slice("Blobs are fun!".as_bytes());
        let sidecar = sidecar.build().unwrap();

        let gas_price = provider.get_gas_price().await.unwrap();
        let eip1559_est = provider.estimate_eip1559_fees().await.unwrap();

        let tx = TransactionRequest {
            max_fee_per_blob_gas: Some(gas_price),
            sidecar: Some(sidecar),
            ..TransactionRequest::default()
                .with_to(alice)
                .with_nonce(0)
                .with_max_fee_per_gas(eip1559_est.max_fee_per_gas)
                .with_max_priority_fee_per_gas(eip1559_est.max_priority_fee_per_gas)
        };

        let pending_tx = provider.send_transaction(tx).await.unwrap();
        let recv_tx = receiver.recv().await.unwrap();

        let tx_with_blobs = match recv_tx {
            ReplaceableOrderPoolCommand::Order(order) => match order.as_ref() {
                Order::Tx(MempoolTx { tx_with_blobs }) => Some(tx_with_blobs.clone()),
                _ => None,
            },
            _ => None,
        }
        .unwrap();

        assert_eq!(tx_with_blobs.hash(), *pending_tx.tx_hash());
        assert_eq!(tx_with_blobs.blobs_len(), 1);

        // send another tx without blobs
        let tx = TransactionRequest::default()
            .with_to(alice)
            .with_nonce(1)
            .with_value(U256::from(1))
            .with_max_fee_per_gas(eip1559_est.max_fee_per_gas)
            .with_max_priority_fee_per_gas(eip1559_est.max_priority_fee_per_gas);

        let pending_tx = provider.send_transaction(tx).await.unwrap();
        let recv_tx = receiver.recv().await.unwrap();

        let tx_without_blobs = match recv_tx {
            ReplaceableOrderPoolCommand::Order(order) => match order.as_ref() {
                Order::Tx(MempoolTx { tx_with_blobs }) => Some(tx_with_blobs.clone()),
                _ => None,
            },
            _ => None,
        }
        .unwrap();

        assert_eq!(tx_without_blobs.hash(), *pending_tx.tx_hash());
        assert_eq!(tx_without_blobs.blobs_len(), 0);
    }
}
