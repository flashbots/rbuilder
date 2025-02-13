use super::ReplaceableOrderPoolCommand;
use crate::preconf::preconf_api_client::PreconfApiClient;
use crate::preconf::preconf_ws_client::PreconfWsClient;
use crate::preconf::{new_preconf_api, new_preconf_ws, PreconfClient, PreconfConfig, PreconfInfo};
use crate::primitives::Order;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tokio::sync::RwLock;
use tokio::time::error::Elapsed;
use tokio::time::timeout;
use tokio::{
    sync::{mpsc, mpsc::error::SendTimeoutError},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace};

const PRECONF_RECEIVER_TIMEOUT_PERIOD: Duration = Duration::from_millis(100);
const PRECONF_SEND_ORDER_TIMEOUT_PERIOD: Duration = Duration::from_millis(50);
const PRECONF_TOKEN_REFRESH_PERIOD: Duration = Duration::from_secs(3350);
const PRECONF_WS_PING_PERIOD: Duration = Duration::from_secs(20);
pub const WS_READ_TIMEOUT_PERIOD: Duration = Duration::from_millis(100);
const PRECONF_CAPACITY: usize = 30_000;

/// Subscribes to EL mempool and pushes new txs as orders in results.
/// This version allows 4844 by subscribing to subscribe_pending_txs to get the hashes and then calling eth_getRawTransactionByHash
/// to get the raw tx that, in case of 4844 tx, may include blobs.
/// In the future we may consider updating reth so we can process blob txs in a different task to avoid slowing down non blob txs.
pub async fn subscribe_to_preconf_pool(
    config: PreconfConfig,
    results: mpsc::Sender<ReplaceableOrderPoolCommand>,
    global_cancel: CancellationToken,
) -> eyre::Result<(Vec<JoinHandle<()>>, mpsc::Sender<PreconfInfo>)> {
    let mut preconf_handlers = Vec::new();
    let (preconf_order_sender, mut preconf_order_receiver) =
        mpsc::channel::<Order>(PRECONF_CAPACITY);
    // handle preconf order receiver
    let order_handler = tokio::spawn(async move {
        info!("Subscribe to preconf pool: started");
        while let Some(order) = preconf_order_receiver.recv().await {
            debug!("received preconf order (id={}).", order.id());
            match results
                .send_timeout(
                    ReplaceableOrderPoolCommand::Order(order.clone()),
                    PRECONF_SEND_ORDER_TIMEOUT_PERIOD,
                )
                .await
            {
                Ok(()) => {
                    trace!("Successfully processed preconf order {}", order.id());
                }
                Err(e) => {
                    error!("Failed to process preconf order {}: {}", order.id(), e);
                    if matches!(e, SendTimeoutError::Closed(_)) {
                        break;
                    }
                }
            }
        }
        global_cancel.cancel();
        info!("Subscribe to preconf pool: finished");
    });
    preconf_handlers.push(order_handler);
    let (preconf_info_sender, rx) = mpsc::channel::<PreconfInfo>(1);
    let api_rx = Arc::new(RwLock::new(rx));
    let ws_rx = Arc::clone(&api_rx);
    info!("Created preconf info channel.");
    let (mut api_client, preconf_ws) =
        get_preconf_client(config.clone(), preconf_order_sender.clone())
            .await
            .unwrap();
    let api_handler: JoinHandle<()> = tokio::spawn(async move {
        let mut last_refresh = Instant::now();
        loop {
            if last_refresh.elapsed() >= PRECONF_TOKEN_REFRESH_PERIOD {
                debug!("preconf fetcher starts to refresh access.");
                api_client.refresh_access().await;
                debug!(
                    "preconf fetcher refreshed access, triggered time={:?}",
                    last_refresh.elapsed()
                );
                last_refresh = Instant::now();
            }
            let is_fallback_enabled = {
                let read_guard = api_client.is_fallback_enabled.read().await;
                *read_guard
            };
            if is_fallback_enabled {
                match timeout(PRECONF_RECEIVER_TIMEOUT_PERIOD, api_rx.write().await.recv()).await {
                    Ok(Some(info)) => {
                        debug!("latest preconf info from api received: {:?}", info.clone());
                        let market_expiry = api_client
                            .get_inclusion_preconf_market_expiry(info.slot.clone())
                            .await;
                        debug!(
                            "let market_expiry: {:?}, now: {:?}",
                            market_expiry,
                            OffsetDateTime::now_utc()
                        );
                        if let Some(expiry) = market_expiry {
                            let slot = info.slot;
                            let block_number = info.block_number;
                            let timestamp = info.timestamp.unwrap();
                            let sleep_duration = expiry - OffsetDateTime::now_utc();
                            if sleep_duration.is_positive() {
                                debug!(
                                    "preconf fetcher scheduling fetch for slot={} after {:?}",
                                    slot, sleep_duration
                                );
                                tokio::time::sleep(sleep_duration.try_into().unwrap()).await;
                            }
                            debug!("preconf fetcher starting fetch for slot={}", slot);
                            api_client
                                .fetch_inclusion_preconfs(slot, block_number, timestamp)
                                .await;
                        } else {
                            debug!(
                                "preconf fetcher did not get market info from {:?}, skip.",
                                info.clone()
                            );
                        }
                    }
                    Err(Elapsed { .. }) => {
                        continue;
                    }
                    Ok(None) => {
                        continue;
                    }
                }
            }
        }
    });
    preconf_handlers.push(api_handler);
    if preconf_ws.is_some() {
        let mut ws_client = preconf_ws.unwrap();
        let ws_handler: JoinHandle<()> = {
            tokio::spawn(async move {
                let mut last_ping = Instant::now();
                let mut curr_preconf_info = PreconfInfo {
                    block_number: 0,
                    slot: 0,
                    timestamp: None,
                };
                ws_client.login(curr_preconf_info).await;
                loop {
                    let start = Instant::now();
                    let is_fallback_enabled = {
                        let read_guard = ws_client.is_fallback_enabled.read().await;
                        *read_guard
                    };
                    if !is_fallback_enabled {
                        if last_ping.elapsed() >= PRECONF_WS_PING_PERIOD {
                            trace!("sending websocket ping...");
                            ws_client.send_ping().await;
                            last_ping = Instant::now();
                            trace!("reset websocket ping timer");
                        }

                        ws_client.read_stream(curr_preconf_info.clone()).await;

                        match timeout(PRECONF_RECEIVER_TIMEOUT_PERIOD, ws_rx.write().await.recv())
                            .await
                        {
                            Ok(Some(info)) => {
                                debug!("latest preconf info from ws received: {:?}", info);
                                curr_preconf_info = info.clone();
                                ws_client
                                    .get_preconf_bundles(Some(curr_preconf_info.slot.clone()))
                                    .await;
                            }
                            Err(Elapsed { .. }) => {
                                continue;
                            }
                            Ok(None) => {
                                continue;
                            }
                        }
                        info!("before read stream, above job took {:?}", start.elapsed());
                    } else {
                        trace!("ws client will re-login.");
                        ws_client
                            .re_login_with_retry(curr_preconf_info.clone())
                            .await;
                    }
                }
            })
        };
        preconf_handlers.push(ws_handler);
    }

    Ok((preconf_handlers, preconf_info_sender))
}

pub async fn get_preconf_client(
    config: PreconfConfig,
    preconf_sender: mpsc::Sender<Order>,
) -> eyre::Result<(PreconfApiClient, Option<PreconfWsClient>)> {
    let preconf_client = PreconfClient {
        market_info: Arc::new(RwLock::new(HashMap::new())),
        access_token: Arc::new(RwLock::new(None)),
        is_fallback_enabled: Arc::new(RwLock::new(false)),
    };
    let api_client = new_preconf_api(&preconf_client, config.clone(), preconf_sender.clone()).await;
    if api_client.is_none() {
        return Err(eyre::eyre!(
            "kindly include preconf_api_url in the configuration"
        ));
    }
    let ws_client = new_preconf_ws(&preconf_client, config.clone(), preconf_sender.clone()).await;
    Ok((api_client.unwrap(), ws_client))
}
