use super::{OrderInputConfig, ReplaceableOrderPoolCommand};
use crate::{
    primitives::{
        serialize::{
            RawBundle, RawBundleDecodeResult, RawShareBundle, RawShareBundleDecodeResult, RawTx,
            TxEncoding,
        },
        BundleReplacementData, BundleReplacementKey, MempoolTx, Order,
    },
    telemetry::mark_command_received,
};
use alloy_primitives::{Address, Bytes};
use jsonrpsee::{server::Server, types::ErrorObject, RpcModule};
use serde::Deserialize;
use std::{
    net::{SocketAddr, SocketAddrV4},
    time::{Duration, Instant},
};
use time::OffsetDateTime;
use tokio::{
    sync::{mpsc, mpsc::error::SendTimeoutError},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, trace, warn};
use uuid::Uuid;

/// Creates a jsonrpsee::server::Server configuring the handling for our RPC calls.
/// Spawns a task that cancels global_cancel if the RPC stops (it's reasonable to shutdown and restart if we don't get orders!).
/// @Pending reengineering to modularize rpc, block_subsidy_selector here is a patch.
pub async fn start_server_accepting_bundles(
    config: OrderInputConfig,
    results: mpsc::Sender<ReplaceableOrderPoolCommand>,
    extra_rpc: RpcModule<()>,
    global_cancel: CancellationToken,
) -> eyre::Result<JoinHandle<()>> {
    let addr = SocketAddr::V4(SocketAddrV4::new(config.server_ip, config.server_port));
    let timeout = config.results_channel_timeout;

    let server = Server::builder()
        .max_connections(config.serve_max_connections)
        .http_only()
        .build(addr)
        .await?;

    let mut module = RpcModule::new(());

    let results_clone = results.clone();
    module.register_async_method("eth_sendBundle", move |params, _| {
        handle_eth_send_bundle(results_clone.clone(), timeout, params)
    })?;

    let results_clone = results.clone();
    module.register_async_method("mev_sendBundle", move |params, _| {
        handle_mev_send_bundle(results_clone.clone(), timeout, params)
    })?;

    let results_clone = results.clone();
    module.register_async_method("eth_cancelBundle", move |params, _| {
        handle_cancel_bundle(results_clone.clone(), timeout, params)
    })?;

    let results_clone = results.clone();
    module.register_async_method("eth_sendRawTransaction", move |params, _| {
        let results = results_clone.clone();
        async move {
	    let received_at = OffsetDateTime::now_utc();
            let start = Instant::now();
            let raw_tx: Bytes = match params.one() {
                Ok(raw_tx) => raw_tx,
                Err(err) => {
                    warn!(?err, "Failed to parse raw transaction");
                    // @Metric
                    return Err(err);
                }
            };
            let raw_tx_order = RawTx { tx: raw_tx };

            let tx: MempoolTx = match raw_tx_order.decode(TxEncoding::WithBlobData) {
                Ok(tx) => tx,
                Err(err) => {
                    warn!(?err, "Failed to decode raw transaction");
                    // @Metric
                    return Err(ErrorObject::owned(-32602, "failed to verify transaction", None::<()>));
                }
            };
            let hash = tx.tx_with_blobs.hash();
            let order = Order::Tx(tx);
            let parse_duration = start.elapsed();
            trace!(order = ?order.id(), parse_duration_mus = parse_duration.as_micros(), "Received mempool tx from API");
            send_order(order, &results, timeout, received_at).await;
            Ok(hash)
        }
    })?;

    module.merge(extra_rpc)?;
    let handle = server.start(module);

    Ok(tokio::spawn(async move {
        info!("RPC server job: started");
        tokio::select! {
            _ = global_cancel.cancelled() => {},
            _ = handle.stopped() => {
                info!("RPC Server stopped");
                global_cancel.cancel();
            },
        }

        info!("RPC server job: finished");
    }))
}

/// Parses a bundle packet and forwards it to the results.
/// Here we can generate:
/// - ReplaceableOrderPoolCommand::Order(Bundle)).
/// - ReplaceableOrderPoolCommand::CancelBundle (identified using empty txs).
async fn handle_eth_send_bundle(
    results: mpsc::Sender<ReplaceableOrderPoolCommand>,
    timeout: Duration,
    params: jsonrpsee::types::Params<'static>,
) {
    let received_at = OffsetDateTime::now_utc();
    let start = Instant::now();
    let raw_bundle: RawBundle = match params.one() {
        Ok(raw_bundle) => raw_bundle,
        Err(err) => {
            warn!(?err, "Failed to parse raw bundle");
            // @Metric
            return;
        }
    };

    let bundle_res = match raw_bundle.decode(TxEncoding::WithBlobData) {
        Ok(bundle_res) => bundle_res,
        Err(err) => {
            warn!(?err, "Failed to decode raw bundle");
            // @Metric
            return;
        }
    };
    match bundle_res {
        RawBundleDecodeResult::NewBundle(bundle) => {
            let order = Order::Bundle(bundle);
            let parse_duration = start.elapsed();
            let target_block = order.target_block().unwrap_or_default();
            trace!(order = ?order.id(), parse_duration_mus = parse_duration.as_micros(), target_block, "Received bundle");
            send_order(order, &results, timeout, received_at).await;
        }
        RawBundleDecodeResult::CancelBundle(replacement_data) => {
            send_command(
                ReplaceableOrderPoolCommand::CancelBundle(replacement_data),
                &results,
                timeout,
                received_at,
            )
            .await;
        }
    }
}

/// Parses a mev share bundle packet and forwards it to the results.
/// Here we can generate ReplaceableOrderPoolCommand::Order(ShareBundle)) or CancelShareBundle (identified using a "cancel" field (a little ugly)).
async fn handle_mev_send_bundle(
    results: mpsc::Sender<ReplaceableOrderPoolCommand>,
    timeout: Duration,
    params: jsonrpsee::types::Params<'static>,
) {
    let received_at = OffsetDateTime::now_utc();
    let start = Instant::now();
    let raw_bundle: RawShareBundle = match params.one() {
        Ok(raw_bundle) => raw_bundle,
        Err(err) => {
            warn!(?err, "Failed to parse raw share bundle");
            // @Metric
            return;
        }
    };
    let decode_res = match raw_bundle.decode(TxEncoding::WithBlobData) {
        Ok(res) => res,
        Err(err) => {
            warn!(?err, "Failed to decode raw share bundle");
            // @Metric
            return;
        }
    };
    match decode_res {
        RawShareBundleDecodeResult::NewShareBundle(bundle) => {
            let order = Order::ShareBundle(*bundle);
            let parse_duration = start.elapsed();
            let target_block = order.target_block().unwrap_or_default();
            trace!(order = ?order.id(), parse_duration_mus = parse_duration.as_micros(), target_block, "Received share bundle");
            send_order(order, &results, timeout, received_at).await;
        }
        RawShareBundleDecodeResult::CancelShareBundle(cancel) => {
            trace!(cancel = ?cancel, "Received share bundle cancellation");
            send_command(
                ReplaceableOrderPoolCommand::CancelShareBundle(cancel),
                &results,
                timeout,
                received_at,
            )
            .await;
        }
    };
}

async fn send_order(
    order: Order,
    channel: &mpsc::Sender<ReplaceableOrderPoolCommand>,
    timeout: Duration,
    received_at: OffsetDateTime,
) {
    send_command(
        ReplaceableOrderPoolCommand::Order(order),
        channel,
        timeout,
        received_at,
    )
    .await;
}

/// Eats the errors and traces them.
async fn send_command(
    command: ReplaceableOrderPoolCommand,
    channel: &mpsc::Sender<ReplaceableOrderPoolCommand>,
    timeout: Duration,
    received_at: OffsetDateTime,
) {
    mark_command_received(&command, received_at);
    match channel.send_timeout(command, timeout).await {
        Ok(()) => {}
        Err(SendTimeoutError::Timeout(_)) => {
            warn!("Failed to sent order, timeout");
        }
        Err(SendTimeoutError::Closed(_)) => {}
    };
}

/// params for eth_cancelBundle
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawCancelBundle {
    pub replacement_uuid: Uuid,
    pub signing_address: Address,
}

/// Parses bundle cancellations a sends CancelBundle to the results.
async fn handle_cancel_bundle(
    results: mpsc::Sender<ReplaceableOrderPoolCommand>,
    timeout: Duration,
    params: jsonrpsee::types::Params<'static>,
) {
    let received_at = OffsetDateTime::now_utc();
    let cancel_bundle: RawCancelBundle = match params.one() {
        Ok(cancel_bundle) => cancel_bundle,
        Err(err) => {
            warn!(?err, "Failed to parse cancel bundle");
            // @Metric
            return;
        }
    };
    let key = BundleReplacementKey::new(
        cancel_bundle.replacement_uuid,
        cancel_bundle.signing_address,
    );
    let sequence_number = 0;
    let replacement_data = BundleReplacementData {
        key,
        sequence_number,
    };
    // @Pending nonce
    send_command(
        ReplaceableOrderPoolCommand::CancelBundle(replacement_data),
        &results,
        timeout,
        received_at,
    )
    .await;
}
