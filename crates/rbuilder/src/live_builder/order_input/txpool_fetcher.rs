use super::{OrderInputConfig, ReplaceableOrderPoolCommand};
use crate::{
    primitives::{MempoolTx, Order, TransactionSignedEcRecoveredWithBlobs},
    telemetry::add_txfetcher_time_to_query,
};
use alloy_primitives::{hex, Bytes, FixedBytes};
use alloy_provider::{IpcConnect, Provider, ProviderBuilder, RootProvider};
use alloy_pubsub::PubSubFrontend;
use futures::StreamExt;
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
    let ipc = IpcConnect::new(config.ipc_path);
    let provider = ProviderBuilder::new().on_ipc(ipc).await?;

    let handle = tokio::spawn(async move {
        info!("Subscribe to txpool with blobs: started");

        let stream = match provider.subscribe_pending_transactions().await {
            Ok(stream) => stream.into_stream().take_until(global_cancel.cancelled()),
            Err(err) => {
                error!(?err, "Failed to subscribe to ipc txpool stream");
                // Closing builder because this job is critical so maybe restart will help
                global_cancel.cancel();
                return;
            }
        };
        let mut stream = pin!(stream);

        while let Some(tx_hash) = stream.next().await {
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

        // stream is closed, cancelling token because builder can't work without this stream
        global_cancel.cancel();
        info!("Subscribe to txpool: finished");
    });

    Ok(handle)
}

/// Calls eth_getRawTransactionByHash on EL node and decodes.
async fn get_tx_with_blobs(
    tx_hash: FixedBytes<32>,
    provider: &RootProvider<PubSubFrontend>,
) -> eyre::Result<Option<TransactionSignedEcRecoveredWithBlobs>> {
    // TODO: Use https://github.com/alloy-rs/alloy/pull/1168 when it gets cut
    // in a release
    let raw_tx: Option<String> = provider
        .client()
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
    use std::{net::Ipv4Addr, path::PathBuf};
    use tokio::time::Duration;

    fn default_config() -> OrderInputConfig {
        OrderInputConfig {
            ipc_path: PathBuf::from("/tmp/anvil.ipc"),
            results_channel_timeout: Duration::new(5, 0),
            ignore_cancellable_orders: false,
            ignore_blobs: false,
            input_channel_buffer_size: 10,
            serve_max_connections: 4096,
            server_ip: Ipv4Addr::new(127, 0, 0, 1),
            server_port: 0,
        }
    }

    #[tokio::test]
    /// Test that the fetcher can retrieve transactions (both normal and blob) from the txpool
    async fn test_fetcher_retrieves_transactions() {
        let anvil = Anvil::new()
            .args(["--ipc", "/tmp/anvil.ipc"])
            .try_spawn()
            .unwrap();

        let (sender, mut receiver) = mpsc::channel(10);
        subscribe_to_txpool_with_blobs(default_config(), sender, CancellationToken::new())
            .await
            .unwrap();

        let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
        let wallet = EthereumWallet::from(signer);

        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(wallet)
            .on_http(anvil.endpoint().parse().unwrap());

        let alice = anvil.addresses()[0];

        let sidecar: SidecarBuilder<SimpleCoder> =
            SidecarBuilder::from_slice("Blobs are fun!".as_bytes());
        let sidecar = sidecar.build().unwrap();

        let gas_price = provider.get_gas_price().await.unwrap();
        let eip1559_est = provider.estimate_eip1559_fees(None).await.unwrap();

        let tx = TransactionRequest::default()
            .with_to(alice)
            .with_nonce(0)
            .with_max_fee_per_blob_gas(gas_price)
            .with_max_fee_per_gas(eip1559_est.max_fee_per_gas)
            .with_max_priority_fee_per_gas(eip1559_est.max_priority_fee_per_gas)
            .with_blob_sidecar(sidecar);

        let pending_tx = provider.send_transaction(tx).await.unwrap();
        let recv_tx = receiver.recv().await.unwrap();

        let tx_with_blobs = match recv_tx {
            ReplaceableOrderPoolCommand::Order(Order::Tx(MempoolTx { tx_with_blobs })) => {
                Some(tx_with_blobs)
            }
            _ => None,
        }
        .unwrap();

        assert_eq!(tx_with_blobs.tx.hash(), *pending_tx.tx_hash());
        assert_eq!(tx_with_blobs.blobs_sidecar.blobs.len(), 1);

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
            ReplaceableOrderPoolCommand::Order(Order::Tx(MempoolTx { tx_with_blobs })) => {
                Some(tx_with_blobs)
            }
            _ => None,
        }
        .unwrap();

        assert_eq!(tx_without_blobs.tx.hash(), *pending_tx.tx_hash());
        assert_eq!(tx_without_blobs.blobs_sidecar.blobs.len(), 0);
    }
}
