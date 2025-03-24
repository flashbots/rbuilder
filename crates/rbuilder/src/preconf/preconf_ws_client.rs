use crate::live_builder::order_input::preconf_fetcher::WS_READ_TIMEOUT_PERIOD;
use crate::preconf::{assign_preconf_ordering, convert_str_to_address, convert_timestamp_ns, eth_to_wei, string_to_uuid, PreconfBundleType, PreconfError, PreconfHealthStatus, PreconfInfo, PreconfReservedInfo, PreconfState};
use crate::primitives::{
    Bundle, BundleReplacementData, BundleReplacementKey, Metadata, Order,
    TransactionSignedEcRecoveredWithBlobs,
};
use alloy_primitives::{hex, keccak256, Bytes, B256};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch, RwLock};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{Error, Message};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, trace, warn};

#[derive(Debug, Serialize, Deserialize)]
struct PreconfWsMessage {
    op: OpCode,
    args: Vec<PreconfArg>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OpCode {
    Query,
    Subscribe,
    Unsubscribe,
    Login,
    Error,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum PreconfArg {
    PreconfChannel(PreconfChannelMessage),
    PreconfQuery(PreconfQueryMessage),
    PreconfLogin(PreconfLoginMessage),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreconfChannelMessage {
    pub channel: PreconfChannel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_type: Option<PreconfMarketType>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreconfQueryMessage {
    pub query_type: PreconfQueryType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_type: Option<PreconfMarketType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreconfLoginMessage {
    pub access_token: String,
}

#[derive(Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
enum PreconfQueryType {
    PreconfMarket,
    PreconfBundles,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum PreconfChannel {
    PreconfMarketUpdate,
    // MarketPriceHistory,
    // RecentTrades,
    // OrderBook,
    // MarketInfo,
    CurrentSlot,
    PreconfBundleUpdate,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum PreconfMarketType {
    InclusionPreconf,
    WholeBlock,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WebsocketEvent {
    Data(DataEvent),
    Log(LogEvent),
    Subscription(SubscriptionEvent),
    Connection(ConnectionEvent),
    QueryResponse(WsQueryResponse),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogEvent {
    event: OpCode,
    code: i32,
    msg: String,
    conn_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscriptionEvent {
    event: OpCode,
    arg: PreconfChannelMessage,
    conn_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionEvent {
    event: String,
    channel: PreconfChannel,
    count: i32,
    conn_id: String,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
struct DataEvent {
    #[serde(rename = "e")]
    event: PreconfChannel,
    #[serde(rename = "E")]
    timestamp: u64,
    #[serde(rename = "s")]
    #[serde(default)]
    instrument_id: Option<String>,
    #[serde(rename = "P")]
    data: WsStreamData,
}

#[derive(Debug, Deserialize)]
struct WsQueryResponse {
    #[serde(rename = "q")]
    query_type: PreconfQueryType,
    #[serde(rename = "P")]
    data: WsQueryData,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
enum WsQueryData {
    PreconfBundles(PreconfBundles),
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
enum WsStreamData {
    CurrentSlot(CurrentSlotData),
    PreconfMarketUpdate(PreconfMarketUpdateData),
    PreconfBundleUpdate(PreconfBundles),
}
#[derive(Debug, Deserialize, Eq, PartialEq, Default)]
struct CurrentSlotData {
    #[serde(rename = "s")]
    slot: u64,
    #[serde(rename = "t")]
    current_time: u64,
    #[serde(rename = "r")]
    remaining_time: u64,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Default)]
pub struct PreconfMarketUpdateData {
    #[serde(rename = "s")]
    slot: u64,
    #[serde(rename = "M")]
    maturity_time: u64,
    #[serde(rename = "a")]
    trx_submit_time: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PreconfBundles {
    #[serde(rename = "s")]
    slot: u64,
    #[serde(rename = "bu")]
    bundles: Vec<PreconfBundle>,
    #[serde(rename = "e")]
    empty_space: i64,
    #[serde(rename = "r", skip_serializing_if = "Option::is_none")]
    fee_recipient: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PreconfBundle {
    #[serde(rename = "u")]
    replacement_uuid: String,
    txs: Vec<PreconfTx>,
    #[serde(rename = "p")]
    bid_price: String,
    #[serde(rename = "o")]
    ordering: Option<i8>,
    #[serde(rename = "B", skip_serializing_if = "Option::is_none")]
    bundle_type: Option<PreconfBundleType>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct PreconfTx {
    #[serde(rename = "tx")]
    tx: String,
    #[serde(rename = "h")]
    tx_hash: String,
    #[serde(rename = "r")]
    can_revert: bool,
}

// To skip PartialEq on PreconfBundles
impl PartialEq for PreconfBundles {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}
// To skip Eq on PreconfBundles
impl Eq for PreconfBundles {}
#[derive(Debug)]
pub struct PreconfWsClient {
    pub ws_url: String,
    pub ws_reader: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    pub ws_writer: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    pub order_sender: mpsc::Sender<Order>,
    pub info_receiver: watch::Receiver<PreconfInfo>,
    pub reserved_sender: watch::Sender<PreconfReservedInfo>,
    pub is_logged_in: bool,
    pub retry_count: u32,
    pub max_retries: u32,
    pub retry_base_delay_ms: u64,
    pub state: PreconfState,
}

impl PreconfWsClient {
    pub async fn login(&mut self) {
        self.send_ping().await;
        let login_text;
        {
            let guard = self.state.access_token.read().await;
            let access_token = guard.clone();
            if access_token.is_none() {
                error!("please login on preconf api client first.");
                return;
            };
            let args = vec![PreconfArg::PreconfLogin(PreconfLoginMessage {
                access_token: access_token.unwrap(),
            })];
            login_text = self.create_ws_message(OpCode::Login, args);
        }
        self.send_message(login_text).await;
        // Wait for login confirmation
        while !self.is_logged_in {
            // debug!("waiting login confirmation...");
            self.read_stream().await;
        }
        self.subscribe().await;
    }

    pub async fn re_login_with_retry(&mut self) {
        while self.retry_count < self.max_retries {
            info!("re-logging in to preconf websocket server...");
            let delay = self.retry_base_delay_ms * (2_u64.pow(self.retry_count));
            tokio::time::sleep(Duration::from_millis(delay)).await;
            let is_connected = self.reconnect().await;
            if is_connected {
                // Re-login successful
                self.retry_count = 0;
                self.login().await;
                if self.is_logged_in {
                    // Move the health_guard into a separate scope
                    {
                        let mut health_guard = self.state.health_status.write().await;
                        *health_guard = PreconfHealthStatus::WsEnabled;
                    } // health_guard is dropped here
                    self.get_preconf_bundles().await;
                    return;
                }
            }
            self.retry_count += 1;
            warn!(
                "Preconf websocket re-login attempt {} failed, retrying in {}ms",
                self.retry_count, delay
            );
        }
        if self.retry_count == self.max_retries {
            error!(
                "Reached the maximum number of attempts ({}). \
            Preconf websocket client is unable to re-login. \
            Please check the connection and restart the builder.",
                self.max_retries
            );
            self.retry_count += 1;
        }
    }

    pub async fn reconnect(&mut self) -> bool {
        match connect_async(self.ws_url.as_str()).await {
            Ok((socket, resp)) => {
                if resp.status().as_u16() == 101 {
                    let (ws_writer, ws_reader) = socket.split();
                    self.ws_reader = ws_reader;
                    self.ws_writer = ws_writer;
                    return true;
                }
            }
            Err(_) => {}
        };
        false
    }

    async fn subscribe(&mut self) {
        let args = vec![
            PreconfArg::PreconfChannel(PreconfChannelMessage {
                channel: PreconfChannel::PreconfMarketUpdate,
                market_type: Some(PreconfMarketType::InclusionPreconf),
            }),
            PreconfArg::PreconfChannel(PreconfChannelMessage {
                channel: PreconfChannel::PreconfBundleUpdate,
                market_type: None,
            }),
        ];
        let subscribe_text: Message = self.create_ws_message(OpCode::Subscribe, args);
        self.send_message(subscribe_text).await;
    }

    async fn send_message(&mut self, msg: Message) {
        match self.ws_writer.send(msg.clone()).await {
            Ok(_) => {
                debug!("Sent ws message: {:?}", msg);
            }
            Err(e) => {
                error!("failed to send ws message: {:?}, Error: {:?}", msg, e);
                let mut guard = self.state.health_status.write().await;
                *guard = PreconfHealthStatus::FallbackEnabled;
                self.is_logged_in = false;
            }
        }
    }

    pub async fn get_preconf_bundles(&mut self) {
        let curr_info = self.info_receiver.borrow_and_update().clone();
        let args = vec![PreconfArg::PreconfQuery(PreconfQueryMessage {
            query_type: PreconfQueryType::PreconfBundles,
            market_type: None,
            slot: Some(curr_info.slot),
        })];
        let query_text = self.create_ws_message(OpCode::Query, args);
        self.send_message(query_text).await;
    }

    fn create_ws_message(&self, op: OpCode, args: Vec<PreconfArg>) -> Message {
        let msg: PreconfWsMessage = PreconfWsMessage { op, args };
        let json_string = serde_json::to_string(&msg).unwrap();
        Message::Text(json_string).into()
    }

    pub async fn send_ping(&mut self) {
        self.send_message(Message::Text("ping".into())).await;
    }

    pub async fn read_stream(&mut self) {
        match timeout(WS_READ_TIMEOUT_PERIOD, self.ws_reader.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                self.on_message(text).await;
            }
            Ok(Some(Ok(Message::Close(_)))) => {
                self.on_close().await;
            }
            Ok(Some(Err(e))) => {
                self.on_error(e).await;
            }
            _ => {
                // trace!("No message received from preconf websocket.");
                ()
            }
        }
    }

    async fn on_message(&mut self, text: String) {
        if text.eq("pong") {
            return;
        }
        // debug!("Received Websocket Text: {}", text);
        match serde_json::from_str::<WebsocketEvent>(&text) {
            Ok(event) => match event {
                WebsocketEvent::Data(DataEvent { event, data, .. }) => match (event, data) {
                    (
                        PreconfChannel::PreconfMarketUpdate,
                        WsStreamData::PreconfMarketUpdate(data),
                    ) => {
                        let market_info = Arc::clone(&self.state.market_info);
                        tokio::spawn(async move {
                            process_preconf_market_update(market_info, data).await;
                        });
                    }
                    (
                        PreconfChannel::PreconfBundleUpdate,
                        WsStreamData::PreconfBundleUpdate(data),
                    ) => {
                        let curr_info = self.info_receiver.borrow_and_update().clone();
                        if data.slot != curr_info.slot {
                            warn!("received preconf bundle stream event slot ({}) is not match with current rpc slot ({}).",data.slot, curr_info.slot);
                        }
                        let sender = self.order_sender.clone();
                        let fee_recipient = data.fee_recipient.clone().map(convert_str_to_address);
                        let reserved_info = PreconfReservedInfo {
                            slot: data.slot.clone(),
                            empty_space: data.empty_space.clone(),
                            fee_recipient,
                        };
                        tokio::spawn(async move {
                            debug!("received ws get bundle stream event: {}", text);
                            process_preconf_bundles(&sender, data, curr_info).await;
                        });
                        self.reserved_sender.send(reserved_info).unwrap();
                    }
                    (PreconfChannel::CurrentSlot, WsStreamData::CurrentSlot(data)) => {
                        trace!("Received current slot event: {:?}", data);
                    }
                    (event, data) => {
                        error!("Mismatched event and data types: {:?} {:?}", event, data);
                    }
                },
                WebsocketEvent::QueryResponse(WsQueryResponse { query_type, data }) => {
                    match (query_type, data) {
                        (PreconfQueryType::PreconfBundles, WsQueryData::PreconfBundles(data)) => {
                            let curr_info = self.info_receiver.borrow_and_update().clone();
                            if data.slot != curr_info.slot {
                                warn!("received preconf bundle query event slot ({}) is not match with current rpc slot ({}).",data.slot, curr_info.slot);
                            }

                            let sender = self.order_sender.clone();
                            let fee_recipient = data.fee_recipient.clone().map(convert_str_to_address);
                            let reserved_info = PreconfReservedInfo {
                                slot: data.slot.clone(),
                                empty_space: data.empty_space.clone(),
                                fee_recipient,
                            };
                            tokio::spawn(async move {
                                debug!("received ws get bundle query response: {}", text);
                                process_preconf_bundles(&sender, data, curr_info).await;
                            });
                            self.reserved_sender.send(reserved_info).unwrap();
                        }
                        (query_type, data) => {
                            error!(
                                "Mismatched query type and data types: {:?} {:?}",
                                query_type, data
                            );
                        }
                    }
                }
                WebsocketEvent::Log(log_event) => {
                    trace!("Received log event: {:?}", log_event);
                    if log_event.event == OpCode::Login {
                        self.is_logged_in = true;
                        info!(
                            "preconf websocket client(id={}) logged in.",
                            log_event.conn_id
                        );
                    } else if log_event.event == OpCode::Error {
                        error!(
                            "received websocket error message={}, code={}",
                            log_event.msg, log_event.code
                        );
                    }
                }
                WebsocketEvent::Subscription(sub_event) => {
                    trace!(
                        "Received subscription event: {:?}, {:?}, {:?}",
                        sub_event.event,
                        sub_event.conn_id,
                        sub_event.arg
                    );
                }
                WebsocketEvent::Connection(connection_event) => {
                    trace!(
                        "Received connection event: {:?}, {:?}, {:?}, {:?}",
                        connection_event.channel,
                        connection_event.conn_id,
                        connection_event.count,
                        connection_event.event
                    );
                }
            },
            Err(e) => {
                error!("Failed to parse websocket message: {}, error={}", text, e);
            }
        }
    }

    pub async fn get_market_info(&self, slot: &u64) -> Option<OffsetDateTime> {
        let guard = self.state.market_info.read().await;
        if guard.contains_key(slot) {
            Some(guard.get(slot).unwrap().clone())
        } else {
            None
        }
    }

    async fn on_close(&mut self) {
        error!("Preconf Websocket connection is closed");
        let mut guard = self.state.health_status.write().await;
        *guard = PreconfHealthStatus::FallbackEnabled;
        self.is_logged_in = false;
    }

    async fn on_error(&mut self, error: Error) {
        match error {
            Error::AlreadyClosed | Error::ConnectionClosed => {
                self.on_close().await;
            }
            _ => {
                error!("Preconf websocket error: {}", error);
            }
        }
    }
}

pub async fn process_preconf_market_update(
    market_info: Arc<RwLock<HashMap<u64, OffsetDateTime>>>,
    market_update: PreconfMarketUpdateData,
) {
    trace!("Received market update event: {:?}", market_update);
    let slot = market_update.slot;
    // skip if market info already have slot info
    let reader = market_info.read().await;

    let mut do_clean = false;
    if reader.len() > 64 {
        do_clean = true;
    }
    drop(reader);
    let mut writer = market_info.write().await;
    // clean expired markets
    if do_clean {
        let til = slot - 1;
        for key in 1..=til {
            writer.remove(&key);
        }
    }
    // append new markets
    let datetime = convert_timestamp_ns(market_update.trx_submit_time);
    writer.insert(slot, datetime);
}

pub async fn process_preconf_bundles(
    order_sender: &mpsc::Sender<Order>,
    data_event: PreconfBundles,
    preconf_info: PreconfInfo,
) {
    for bundle in data_event.bundles {
        if bundle.txs.is_empty() {
            debug!("received preconf transactions is empty");
            return;
        }
        match generate_order_from_ws_preconf(
            preconf_info.block_number,
            preconf_info.timestamp.unwrap(),
            bundle.clone(),
        ) {
            Ok(preconf_order) => {
                debug!("Attempting to send preconf order: {:?}", preconf_order);
                match order_sender.send(preconf_order).await {
                    Ok(_) => {
                        debug!("Successfully sent preconf order.");
                    }
                    Err(err) => {
                        error!("Failed to send preconf order: {}", err);
                    }
                }
            }
            Err(err) => {
                error!("Failed to generate order from preconf: {}", err);
            }
        }
    }
}

pub fn generate_order_from_ws_preconf(
    block: u64,
    timestamp: u64,
    bundle: PreconfBundle,
) -> Result<Order, PreconfError> {
    let mut metadata: Metadata = Metadata::with_current_received_at();
    let mut raw_bundle_hash: Vec<u8> = vec![];
    let mut reverting_tx_hashes: Vec<B256> = vec![];
    let mut signer = None;
    let bundle_uuid = string_to_uuid(bundle.replacement_uuid.clone())?;
    let trxs: Vec<TransactionSignedEcRecoveredWithBlobs> = bundle
        .txs
        .iter()
        .filter_map(|preconf_tx| {
            let tx_bytes = hex::decode(&preconf_tx.tx.clone().trim_start_matches("0x")).unwrap();
            let raw_tx = Bytes::from(tx_bytes);
            // handle transaction with/without blobs
            match TransactionSignedEcRecoveredWithBlobs::decode_enveloped_with_real_blobs(raw_tx) {
                Ok(tx) => {
                    if preconf_tx.can_revert {
                        reverting_tx_hashes.push(tx.hash());
                    };
                    raw_bundle_hash.extend_from_slice(&tx.hash().0.to_vec());
                    signer = Some(tx.signer());
                    Some(tx)
                }
                Err(e) => {
                    error!("cannot decode preconf tx with blobs: {}", e);
                    None
                }
            }
        })
        .collect();

    let replacement_data = BundleReplacementData {
        key: BundleReplacementKey::new(bundle_uuid, signer.unwrap()),
        sequence_number: 0,
    };
    let bundle_hash = keccak256(raw_bundle_hash);
    let bid_price: f64 = bundle.bid_price.parse().unwrap();
    let preconf_ordering = assign_preconf_ordering(bundle.ordering);
    match eth_to_wei(bid_price) {
        Ok(p) => {
            metadata.preconf_bid_price = Some(p);
            metadata.preconf_ordering = Some(preconf_ordering);
            Ok(Order::Bundle(Bundle {
                block,
                min_timestamp: Some(timestamp),
                max_timestamp: None,
                txs: trxs,
                reverting_tx_hashes,
                hash: bundle_hash,
                uuid: bundle_uuid,
                replacement_data: Some(replacement_data),
                signer,
                metadata,
            }))
        }
        Err(e) => Err(PreconfError::PreconfConvertError(format!(
            "Cannot generate order from bundle: {}",
            e
        ))),
    }
}
