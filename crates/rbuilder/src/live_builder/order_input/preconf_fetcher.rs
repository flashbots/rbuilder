use super::ReplaceableOrderPoolCommand;
use crate::preconf::preconf_api_client::PreconfApiClient;
use crate::preconf::preconf_ws_client::PreconfWsClient;
use crate::preconf::{
    new_preconf_api, new_preconf_ws, PreconfConfig, PreconfHealthStatus, PreconfInfo,
    PreconfReservedInfo, PreconfState,
};
use crate::primitives::Order;
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio::{
    sync::{mpsc, mpsc::error::SendTimeoutError, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

pub const PRECONF_RECEIVER_TIMEOUT_PERIOD: Duration = Duration::from_millis(100);
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
) -> eyre::Result<(
    Vec<JoinHandle<()>>,
    watch::Sender<PreconfInfo>,
    watch::Receiver<PreconfReservedInfo>,
    PreconfState,
)> {
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
    info!("Created preconf channel.");
    let (info_sender, info_receiver) = watch::channel(PreconfInfo {
        slot: 0,
        block_number: 0,
        timestamp: None,
    });
    let (reserved_sender, reserved_receiver) = watch::channel(PreconfReservedInfo {
        slot: 0,
        empty_space: 0,
        fee_recipient: None,
    });
    let (mut api_client, preconf_ws, preconf_state) =
        get_preconf_client(config, preconf_order_sender, info_receiver, reserved_sender).await?;
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

            let run_api = {
                let guard = api_client.state.health_status.read().await;
                let health_status = guard.clone();
                drop(guard);
                if health_status == PreconfHealthStatus::FallbackEnabled
                    || health_status == PreconfHealthStatus::ApiEnabled
                {
                    true
                } else if health_status == PreconfHealthStatus::ServerFailed {
                    api_client.re_login().await
                } else {
                    false
                }
            };
            if run_api {
                match timeout(
                    PRECONF_RECEIVER_TIMEOUT_PERIOD,
                    api_client.info_receiver.changed(),
                )
                .await
                {
                    Ok(Ok(())) => {
                        let curr_info = *api_client.info_receiver.borrow_and_update();
                        debug!("Received new preconf info: {:?}", curr_info);
                        if let Some(market_expiry) = api_client
                            .get_inclusion_preconf_market_expiry(curr_info.slot)
                            .await
                        {
                            let sleep_duration = market_expiry - OffsetDateTime::now_utc();
                            if sleep_duration.is_positive() {
                                debug!(
                                    "preconf fetcher scheduling fetch slot={} after {:?} on api",
                                    curr_info.slot, sleep_duration
                                );
                                tokio::time::sleep(sleep_duration.try_into().unwrap()).await;
                            }
                            debug!(
                                "preconf fetcher starting fetch slot={} on api",
                                curr_info.slot
                            );
                            api_client.fetch_inclusion_preconfs(curr_info).await;
                        } else {
                            warn!(
                                "preconf fetcher did not get market info from {:?}(market may not open), skip.",
                                curr_info
                            );
                            let gas_info = PreconfReservedInfo {
                                slot: curr_info.slot,
                                empty_space: 0,
                                fee_recipient: None, //suppose will use the fee recipient address from relay
                            };
                            api_client.reserved_sender.send(gas_info).unwrap();
                        }
                    }
                    Ok(Err(recv_err)) => {
                        // The watch sender has been dropped; exit the loop
                        error!(
                            "preconf info sender in api has been dropped with error={}",
                            recv_err
                        );
                        continue;
                    }
                    Err(_) => {
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
                let query_lock = Arc::new(Mutex::new(false));
                ws_client.login().await;
                loop {
                    let start = Instant::now();
                    let run_ws = {
                        let guard = ws_client.state.health_status.read().await;
                        *guard == PreconfHealthStatus::WsEnabled
                    };
                    if run_ws {
                        if last_ping.elapsed() >= PRECONF_WS_PING_PERIOD {
                            ws_client.send_ping().await;
                            last_ping = Instant::now();
                        }
                        let mut do_query = query_lock.lock().await;
                        if *do_query {
                            *do_query = false;
                            ws_client.get_preconf_bundles().await;
                        }
                        ws_client.read_stream().await;
                        match timeout(
                            PRECONF_RECEIVER_TIMEOUT_PERIOD,
                            ws_client.info_receiver.changed(),
                        )
                        .await
                        {
                            Ok(Ok(())) => {
                                let curr_info = *ws_client.info_receiver.borrow_and_update();
                                if let Some(market_expiry) =
                                    ws_client.get_market_info(&curr_info.slot).await
                                {
                                    let call_query = Arc::clone(&query_lock);
                                    tokio::spawn(async move {
                                        let sleep_duration =
                                            market_expiry - OffsetDateTime::now_utc();
                                        if sleep_duration.is_positive() {
                                            debug!("preconf fetcher scheduling fetch slot={} after {:?} on ws", curr_info.slot, sleep_duration);
                                            tokio::time::sleep(sleep_duration.try_into().unwrap())
                                                .await;
                                        }
                                        debug!(
                                            "preconf fetcher starting fetch slot={} on ws",
                                            curr_info.slot
                                        );
                                        let mut do_query = call_query.lock().await;
                                        *do_query = true;
                                    });
                                }
                            }
                            Ok(Err(recv_err)) => {
                                // The watch sender has been dropped; exit the loop
                                debug!(
                                    "Preconf info sender in ws has been dropped with error={}",
                                    recv_err
                                );
                                continue;
                            }
                            Err(_) => {
                                continue;
                            }
                        }
                        info!("before read stream, above job took {:?}", start.elapsed());
                    } else {
                        trace!("ws client will re-login.");
                        ws_client.re_login_with_retry().await;
                    }
                }
            })
        };
        preconf_handlers.push(ws_handler);
    }

    Ok((
        preconf_handlers,
        info_sender,
        reserved_receiver,
        preconf_state,
    ))
}

pub async fn get_preconf_client(
    config: PreconfConfig,
    preconf_sender: mpsc::Sender<Order>,
    info_receiver: watch::Receiver<PreconfInfo>,
    reserved_sender: watch::Sender<PreconfReservedInfo>,
) -> eyre::Result<(PreconfApiClient, Option<PreconfWsClient>, PreconfState)> {
    if config.fallback_fee_recipient.is_none() {
        return Err(eyre::eyre!(
            "kindly include fallback_fee_recipient in the configuration"
        ));
    }
    let preconf_state = PreconfState::new(config.fallback_fee_recipient.clone().unwrap());
    let api_client = new_preconf_api(
        &preconf_state,
        config.clone(),
        preconf_sender.clone(),
        info_receiver.clone(),
        reserved_sender.clone(),
    )
    .await;
    if api_client.is_none() {
        return Err(eyre::eyre!(
            "kindly include preconf_api_url in the configuration"
        ));
    }
    let ws_client = new_preconf_ws(
        &preconf_state,
        config.clone(),
        preconf_sender.clone(),
        info_receiver.clone(),
        reserved_sender.clone(),
    )
    .await;
    Ok((api_client.unwrap(), ws_client, preconf_state))
}
