pub mod preconf_api_client;
pub mod preconf_ws_client;

use crate::live_builder::config::{Config};
use crate::preconf::preconf_api_client::PreconfApiClient;
use crate::preconf::preconf_ws_client::PreconfWsClient;
use crate::primitives::Order;
use alloy_primitives::{Address, U256};
use futures_util::StreamExt;
use jsonrpsee::core::Serialize;
use reqwest::StatusCode;
use serde::{de, Deserialize, Deserializer};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::{mpsc, watch, RwLock};
use tokio_tungstenite::connect_async;
use tracing::{error, warn};
use url::Url;
use uuid::Uuid;

#[derive(Debug, PartialEq, Clone)]
pub enum PreconfHealthStatus {
    Init = 0,
    ApiEnabled = 1,
    WsEnabled = 2,
    FallbackEnabled = 3,
    ServerFailed = 4,
}

#[derive(Debug, PartialEq, Clone)]
pub enum PreconfOrdering {
    TopPreconf = 4,
    BottomPreconf = 3,
    RegularPreconf = 2,
    PayoutPreconf = 1,
}

#[derive(Debug, Serialize, Clone)]
pub enum PreconfBundleType {
    Regular = 1,
    MEV = 2,
}

impl<'de> Deserialize<'de> for PreconfBundleType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value: u8 = u8::deserialize(deserializer)?;
        match value {
            1 => Ok(PreconfBundleType::Regular),
            2 => Ok(PreconfBundleType::MEV),
            _ => Err(de::Error::custom("invalid value for BundleType")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PreconfReservedInfo {
    pub slot: u64,
    pub empty_space: i64,
    pub fee_recipient: Option<Address>,
}

#[derive(Debug)]
pub struct PreconfState {
    pub fallback_fee_recipient: Address,
    pub market_info: Arc<RwLock<HashMap<u64, OffsetDateTime>>>,
    pub access_token: Arc<RwLock<Option<String>>>,
    pub health_status: Arc<RwLock<PreconfHealthStatus>>,
}

impl PreconfState {
    pub fn new(fallback_fee_recipient: String) -> PreconfState {
        Self {
            market_info: Arc::new(RwLock::new(HashMap::new())),
            access_token: Arc::new(RwLock::new(None)),
            health_status: Arc::new(RwLock::new(PreconfHealthStatus::Init)),
            fallback_fee_recipient: convert_str_to_address(fallback_fee_recipient),
        }
    }

    pub async fn is_healthy(&self) -> bool {
        let guard = self.health_status.read().await;
        if *guard == PreconfHealthStatus::ApiEnabled
            || *guard == PreconfHealthStatus::WsEnabled
            || *guard == PreconfHealthStatus::FallbackEnabled
        {
            return true;
        }
        false
    }

    pub fn get_fallback_fee_recipient(&self) -> Address {
        self.fallback_fee_recipient.clone()
    }
}

impl Clone for PreconfState {
    fn clone(&self) -> Self {
        Self {
            fallback_fee_recipient: self.fallback_fee_recipient.clone(),
            market_info: Arc::clone(&self.market_info),
            access_token: Arc::clone(&self.access_token),
            health_status: Arc::clone(&self.health_status),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct PreconfInfo {
    pub slot: u64,
    pub block_number: u64,
    pub timestamp: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct PreconfConfig {
    // api
    pub preconf_api_url: Option<String>,

    // websocket
    pub preconf_ws_url: Option<String>,

    pub fallback_fee_recipient: Option<String>,

    // login
    relay_secret_key: String,
}

impl PreconfConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        preconf_api_url: Option<String>,
        preconf_ws_url: Option<String>,
        fallback_fee_recipient: Option<String>,
        relay_secret_key: String,
    ) -> Self {
        Self {
            preconf_api_url,
            preconf_ws_url,
            fallback_fee_recipient,
            relay_secret_key,
        }
    }
    pub fn from_config(config: &Config) -> Self {
        PreconfConfig {
            preconf_api_url: config.base_config.preconf_api_url.clone(),
            preconf_ws_url: config.base_config.preconf_ws_url.clone(),
            fallback_fee_recipient: config.base_config.fallback_fee_recipient.clone(),
            relay_secret_key: config.l1_config.get_relay_secret_key().unwrap(),
        }
    }
}

#[derive(Error, Debug, Clone, Serialize, Deserialize)]
pub struct PreconfErrorResponse {
    code: Option<u64>,
    message: String,
}

impl std::fmt::Display for PreconfErrorResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Preconf error: (code: {}, message: {})",
            self.code.unwrap_or_default(),
            self.message
        )
    }
}

#[derive(Error, Debug)]
pub enum PreconfError {
    #[error("Request error: {0}")]
    RequestError(#[from] reqwest::Error),
    #[error("Header error")]
    InvalidHeader,
    #[error("Preconf error: {0}")]
    PreconfError(#[from] PreconfErrorResponse),
    #[error("Unknown preconf response, status: {0}, body: {1}")]
    UnknownPreconfError(StatusCode, String),
    #[error("Too many requests")]
    TooManyRequests,
    #[error("Connection error")]
    ConnectionError,
    #[error("Internal error")]
    InternalError,
    #[error("Preconf order conversion error: {0}")]
    PreconfConvertError(String),
    #[error("Sender Error")]
    SenderError,
}

pub async fn new_preconf_ws(
    preconf_state: &PreconfState,
    config: PreconfConfig,
    order_sender: mpsc::Sender<Order>,
    info_receiver: watch::Receiver<PreconfInfo>,
    reserved_sender: watch::Sender<PreconfReservedInfo>,
) -> Option<PreconfWsClient> {
    if config.preconf_ws_url.is_none() {
        return None;
    }
    let ws_url = config.preconf_ws_url.unwrap();
    let (socket, response) = match connect_async(ws_url.as_str()).await {
        Ok((s, r)) => (s, r),
        Err(_) => {
            warn!("preconf ws cannot connect to {:?}", ws_url);
            return None;
        }
    };
    if response.status().as_u16() != 101 {
        warn!(
            "preconf ws ({:?}) connection response was failed: {:?}",
            ws_url, response
        );
        return None;
    }
    let mut health = preconf_state.health_status.write().await;
    *health = PreconfHealthStatus::WsEnabled;
    let (ws_writer, ws_reader) = socket.split();
    Some(PreconfWsClient {
        ws_url: ws_url.clone(),
        ws_reader,
        ws_writer,
        order_sender,
        info_receiver,
        reserved_sender,
        is_logged_in: false,
        retry_count: 0,
        max_retries: 10,
        retry_base_delay_ms: 2000,
        state: preconf_state.clone(),
    })
}

pub async fn new_preconf_api(
    preconf_state: &PreconfState,
    config: PreconfConfig,
    order_sender: mpsc::Sender<Order>,
    info_receiver: watch::Receiver<PreconfInfo>,
    reserved_sender: watch::Sender<PreconfReservedInfo>,
) -> Option<PreconfApiClient> {
    let mut health = preconf_state.health_status.write().await;
    if config.preconf_api_url.is_none() {
        *health = PreconfHealthStatus::ServerFailed;
        error!("No preconf api url specified");
        return None;
    }

    let api_url = Url::parse(config.preconf_api_url.as_ref().unwrap().as_str()).unwrap();
    let relay_secret_key = config.relay_secret_key.clone();
    let mut api_client = PreconfApiClient {
        api_url,
        client: reqwest::Client::new(),
        refresh_token: None,
        access_token_exp: None,
        refresh_token_exp: None,
        order_sender,
        info_receiver,
        reserved_sender,
        state: preconf_state.clone(),
        relay_secret_key,
    };
    let logged_in = api_client.login().await;
    if logged_in {
        *health = PreconfHealthStatus::ApiEnabled;
    } else {
        *health = PreconfHealthStatus::ServerFailed;
    }
    Some(api_client)
}

pub fn string_to_uuid(uuid_str: String) -> Result<Uuid, PreconfError> {
    match Uuid::from_str(uuid_str.as_str()) {
        Ok(bundle_uuid) => Ok(bundle_uuid),
        Err(_) => {
            let err_msg = format!(
                "Failed to parse UUIDv4 from received uuid str={}",
                uuid_str.as_str()
            );
            Err(PreconfError::PreconfConvertError(err_msg))
        }
    }
}

pub fn eth_to_wei(price_eth: f64) -> Result<U256, PreconfError> {
    if price_eth < 0.0 {
        return Err(PreconfError::PreconfConvertError(String::from(
            "ETH price cannot be negative",
        )));
    }

    // Convert ETH price to Wei
    let wei_value = (price_eth * 1_000_000_000_000_000_000.0).round() as u128; // Multiply by 10^18 and round
    Ok(U256::from(wei_value))
}

pub fn assign_preconf_ordering(ordering: Option<i8>) -> U256 {
    match ordering {
        Some(1) => U256::from(PreconfOrdering::TopPreconf as u64),
        Some(-1) => U256::from(PreconfOrdering::BottomPreconf as u64),
        Some(-2) => U256::from(PreconfOrdering::PayoutPreconf as u64),
        _ => U256::from(PreconfOrdering::RegularPreconf as u64),
    }
}

pub fn convert_timestamp_ns(timestamp: u64) -> OffsetDateTime {
    let ts = timestamp / 1000; // the timestamp is in milliseconds
    OffsetDateTime::from_unix_timestamp(ts as i64).unwrap()
}

pub fn convert_str_to_address(address: String) -> Address {
    Address::from_str(address.as_str()).unwrap()
}
