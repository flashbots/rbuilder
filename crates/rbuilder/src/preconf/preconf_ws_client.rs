#![allow(unused)]
use crate::live_builder::order_input::preconf_fetcher::WS_READ_TIMEOUT_PERIOD;
use crate::preconf::{eth_to_wei, string_to_uuid, PreconfError, PreconfInfo};
use crate::primitives::{
    Bundle, BundleReplacementData, BundleReplacementKey, Metadata, Order,
    TransactionSignedEcRecoveredWithBlobs,
};
use alloy_primitives::{hex, Bytes, B256};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc::Sender;
use tokio::sync::RwLock;
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
    PreConfMarket,
    PreconfBundles,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum PreconfChannel {
    PreConfMarketUpdate,
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
    Subscription(SubscriptionEvent),
    Data(DataEvent),
    Log(LogEvent),
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
    PreConfMarketUpdate(PreConfMarketUpdateData),
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
pub struct PreConfMarketUpdateData {
    #[serde(rename = "s")]
    slot: u64,
    #[serde(rename = "M")]
    maturity_time: u64,
    #[serde(rename = "a")]
    trx_submit_time: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PreconfBundles {
    #[serde(rename = "s")]
    slot: u64,
    #[serde(rename = "bu")]
    bundles: Vec<PreconfBundle>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PreconfBundle {
    #[serde(rename = "u")]
    replacement_uuid: String,
    txs: Vec<PreconfTx>,
    #[serde(rename = "p")]
    average_bid_price: String,
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
    fn eq(&self, other: &Self) -> bool {
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
    pub order_sender: Sender<Order>,
    pub market_info: Arc<RwLock<HashMap<u64, u64>>>, // slot as key, utc timestamp as value
    pub access_token: Arc<RwLock<Option<String>>>,
    pub is_logged_in: bool,
    pub is_fallback_enabled: Arc<RwLock<bool>>,
    pub retry_count: u32,
    pub max_retries: u32,
    pub retry_base_delay_ms: u64,
}

impl PreconfWsClient {
    pub async fn login(&mut self, curr_preconf_info: PreconfInfo) {
        self.send_ping().await;
        let login_text;
        {
            let access_token_reader = self.access_token.read().await;
            let access_token = access_token_reader.clone().unwrap();
            let args = vec![PreconfArg::PreconfLogin(PreconfLoginMessage {
                access_token,
            })];
            login_text = self.create_ws_message(OpCode::Login, args);
        }
        self.send_message(login_text).await;
        // Wait for login confirmation
        while !self.is_logged_in {
            debug!("waiting login confirmation...");
            self.read_stream(curr_preconf_info.clone()).await;
        }
        self.subscribe().await;
    }

    pub async fn re_login_with_retry(&mut self, curr_preconf_info: PreconfInfo) {
        while self.retry_count < self.max_retries {
            info!("re-logging in to preconf websocket server...");
            let delay = self.retry_base_delay_ms * (2_u64.pow(self.retry_count));
            tokio::time::sleep(Duration::from_millis(delay)).await;
            self.re_login().await;
            let retry;
            {
                let fallback_enabled = self.is_fallback_enabled.read().await;
                retry = *fallback_enabled;
            }
            if !retry {
                // Re-login successful
                self.retry_count = 0;
                self.login(curr_preconf_info.clone()).await;
                if self.is_logged_in {
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

    pub async fn re_login(&mut self) {
        match connect_async(self.ws_url.as_str()).await {
            Ok((socket, resp)) => {
                if resp.status().as_u16() != 101 {
                    let mut guard = self.is_fallback_enabled.write().await;
                    *guard = true;
                    return;
                }
                let (ws_writer, ws_reader) = socket.split();
                self.ws_reader = ws_reader;
                self.ws_writer = ws_writer;
                let mut guard = self.is_fallback_enabled.write().await;
                *guard = false;
            }
            Err(_) => {
                let mut guard = self.is_fallback_enabled.write().await;
                *guard = true;
                return;
            }
        };
    }

    async fn subscribe(&mut self) {
        let args = vec![
            PreconfArg::PreconfChannel(PreconfChannelMessage {
                channel: PreconfChannel::PreConfMarketUpdate,
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
                let mut guard = self.is_fallback_enabled.write().await;
                *guard = true;
                self.is_logged_in = false;
            }
        }
    }

    pub async fn get_preconf_bundles(&mut self, slot: Option<u64>) {
        let args = vec![PreconfArg::PreconfQuery(PreconfQueryMessage {
            query_type: PreconfQueryType::PreconfBundles,
            market_type: None,
            slot,
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

    pub async fn read_stream(&mut self, curr_preconf_info: PreconfInfo) {
        match timeout(WS_READ_TIMEOUT_PERIOD, self.ws_reader.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                self.on_message(text, curr_preconf_info).await;
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

    async fn on_message(&mut self, text: String, curr_preconf_info: PreconfInfo) {
        if text.eq("pong") {
            return;
        }
        debug!("Received Websocket Text: {}", text);
        match serde_json::from_str::<WebsocketEvent>(&text) {
            Ok(event) => match event {
                WebsocketEvent::Data(DataEvent { event, data, .. }) => match (event, data) {
                    (
                        PreconfChannel::PreConfMarketUpdate,
                        WsStreamData::PreConfMarketUpdate(data),
                    ) => {
                        let curr_market_info = Arc::clone(&self.market_info);
                        tokio::spawn(async move {
                            process_preconf_market_update(curr_market_info, data).await;
                        });
                    }
                    (
                        PreconfChannel::PreconfBundleUpdate,
                        WsStreamData::PreconfBundleUpdate(data),
                    ) => {
                        let sender = self.order_sender.clone();
                        tokio::spawn(async move {
                            debug!("Received Websocket Get Bundles Stream: {}", text);
                            process_preconf_bundles(&sender, data, curr_preconf_info).await;
                        });
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
                            let sender = self.order_sender.clone();
                            tokio::spawn(async move {
                                debug!("Received Websocket Get Bundles Stream: {}", text);
                                process_preconf_bundles(&sender, data, curr_preconf_info).await;
                            });
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
                        info!("preconf websocket client logged in.");
                    } else if log_event.event == OpCode::Error {
                        error!("received websocket error: {:?}", log_event);
                    }
                }
                WebsocketEvent::Subscription(sub_event) => {
                    trace!("Received subscription event: {:?}", sub_event);
                }
                WebsocketEvent::Connection(connection_event) => {
                    trace!("Received connection event: {:?}", connection_event);
                }
            },
            Err(e) => {
                error!("Failed to parse websocket message: {}, error={}", text, e);
            }
        }
    }

    async fn on_close(&mut self) {
        error!("Preconf Websocket connection is closed");
        let mut writer = self.is_fallback_enabled.write().await;
        *writer = true;
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
    market_info: Arc<RwLock<HashMap<u64, u64>>>,
    market_update: PreConfMarketUpdateData,
) {
    trace!("Received market update event: {:?}", market_update);
    let slot = market_update.slot;
    // skip if market info already have slot info
    let reader = market_info.read().await;
    if reader.contains_key(&slot) || reader.len() == 0 || reader.len() < 64 {
        return;
    }
    // clean expired markets
    let mut writer = market_info.write().await;
    let til = slot - 1;
    for key in 1..=til {
        writer.remove(&key);
    }
    // append new markets
    writer.insert(slot, market_update.trx_submit_time);
}

pub async fn process_preconf_bundles(
    preconf_sender: &Sender<Order>,
    data_event: PreconfBundles,
    curr_preconf_info: PreconfInfo,
) {
    if data_event.slot == curr_preconf_info.slot {
        for bundle in data_event.bundles {
            if bundle.txs.is_empty() {
                debug!("received preconf transactions is empty");
                return;
            }
            debug!("ws: generate_order_from_preconf");
            match generate_ws_order_from_preconf(
                curr_preconf_info.block_number,
                curr_preconf_info.timestamp.unwrap(),
                bundle.clone(),
            ) {
                Ok(preconf_order) => {
                    debug!("Attempting to send preconf order: {:?}", preconf_order);
                    match preconf_sender.send(preconf_order).await {
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
    } else {
        warn!(
            "received preconf bundle event slot ({}) is not match with current rpc slot ({}).",
            data_event.slot, curr_preconf_info.slot
        );
    }
}

pub fn generate_ws_order_from_preconf(
    block: u64,
    timestamp: u64,
    bundle: PreconfBundle,
) -> Result<Order, PreconfError> {
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
                    signer = Some(tx.signer());
                    Some(tx)
                }
                Err(e) => {
                    error!("cannot decode preconf tx with blobs: {}", e);
                    None
                }
            }
            // let tx_bytes =
            //     hex::decode(&preconf_tx.tx.clone().trim_start_matches("0x")).unwrap();
            // let trx = match TransactionSigned::decode(&mut tx_bytes.as_slice()) {
            //     Ok(tx_signed) => tx_signed,
            //     Err(err) => {
            //         error!("cannot convert to signed transaction object, {}", err);
            //         return None;
            //     }
            // };
            // if preconf_tx.can_revert {
            //     reverting_tx_hashes.push(trx.hash);
            // };
            // signer = trx.recover_signer();
            // TransactionSignedEcRecoveredWithBlobs::new_no_blobs(
            //     TransactionSignedEcRecovered::from_signed_transaction(trx, signer.unwrap()),
            // )
        })
        .collect();

    let replacement_data = BundleReplacementData {
        key: BundleReplacementKey::new(bundle_uuid, signer.unwrap()),
        sequence_number: 0,
    };

    let mut metadata: Metadata = Default::default();
    let avg_bid_price: f64 = bundle.average_bid_price.parse().unwrap();
    match eth_to_wei(avg_bid_price) {
        Ok(p) => {
            metadata.avg_bid_price = Some(p);
            Ok(Order::Bundle(Bundle {
                block,
                min_timestamp: Some(timestamp),
                max_timestamp: None,
                txs: trxs,
                reverting_tx_hashes,
                hash: B256::default(),
                uuid: bundle_uuid, //Uuid::new_v4(),
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
