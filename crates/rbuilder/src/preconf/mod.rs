pub mod preconf_api_client;
pub mod preconf_ws_client;

use crate::live_builder::base_config::BaseConfig;
use crate::preconf::preconf_api_client::PreconfApiClient;
use crate::preconf::preconf_ws_client::PreconfWsClient;
use crate::primitives::Order;
use alloy_primitives::U256;
use futures_util::StreamExt;
use jsonrpsee::core::Serialize;
use reqwest::StatusCode;
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc::Sender;
use tokio::sync::RwLock;
use tokio_tungstenite::connect_async;
use tracing::error;
use url::Url;
use uuid::Uuid;

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct PreconfInfo {
    pub block_number: u64,
    pub slot: u64,
    pub timestamp: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct PreconfConfig {
    // api
    pub preconf_api_url: Option<String>,
    pub preconf_chain_id: Option<String>,

    // websocket
    pub preconf_ws_url: Option<String>,

    // login
    relay_secret_key: String,
}

impl PreconfConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        preconf_api_url: Option<String>,
        preconf_chain_id: Option<String>,
        preconf_ws_url: Option<String>,
        relay_secret_key: String,
    ) -> Self {
        Self {
            preconf_api_url,
            preconf_chain_id,
            preconf_ws_url,
            relay_secret_key,
        }
    }
    pub fn from_config(config: &BaseConfig) -> Self {
        PreconfConfig {
            preconf_api_url: config.preconf_api_url.clone(),
            preconf_chain_id: config.preconf_chain_id.clone(),
            preconf_ws_url: config.preconf_ws_url.clone(),
            relay_secret_key: config.get_relay_secret_key().unwrap(),
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

#[derive(Debug)]
pub struct PreconfClient {
    pub market_info: Arc<RwLock<HashMap<u64, u64>>>,
    pub access_token: Arc<RwLock<Option<String>>>,
    pub is_fallback_enabled: Arc<RwLock<bool>>,
}

pub async fn new_preconf_ws(
    preconf_client: &PreconfClient,
    config: PreconfConfig,
    order_sender: Sender<Order>,
) -> Option<PreconfWsClient> {
    let mut ws_client = None;
    if let Some(ws_url) = &config.preconf_ws_url {
        let (socket, response) = connect_async(ws_url.as_str())
            .await
            .expect("Cannot initialize preconf client to connect EthGas websocket");
        if response.status().as_u16() != 101 {
            let mut fallback_enabled = preconf_client.is_fallback_enabled.write().await;
            *fallback_enabled = true;
        } else {
            let (ws_writer, ws_reader) = socket.split();
            ws_client = Some(PreconfWsClient {
                ws_url: ws_url.clone(),
                ws_reader,
                ws_writer,
                order_sender,
                market_info: Arc::clone(&preconf_client.market_info),
                access_token: Arc::clone(&preconf_client.access_token),
                is_logged_in: false,
                is_fallback_enabled: Arc::clone(&preconf_client.is_fallback_enabled),
                retry_count: 0,
                max_retries: 10,
                retry_base_delay_ms: 2000,
            });
        }
    } else {
        let mut fallback_enabled = preconf_client.is_fallback_enabled.write().await;
        *fallback_enabled = true;
    }
    ws_client
}

pub async fn new_preconf_api(
    preconf_client: &PreconfClient,
    config: PreconfConfig,
    order_sender: Sender<Order>,
) -> Option<PreconfApiClient> {
    if config.preconf_api_url.is_none() {
        error!("No preconf api url specified");
        return None;
    }
    if config.preconf_ws_url.is_none() {
        let mut is_fallback_enabled = preconf_client.is_fallback_enabled.write().await;
        *is_fallback_enabled = true;
    }
    let api_url = Url::parse(config.preconf_api_url.as_ref().unwrap().as_str()).unwrap();
    let chain_id = config.preconf_chain_id.unwrap();
    let relay_secret_key = config.relay_secret_key.clone();
    let access_token = Arc::clone(&preconf_client.access_token);
    let api_market_info = Arc::clone(&preconf_client.market_info);
    let is_fallback_enabled = Arc::clone(&preconf_client.is_fallback_enabled);
    let mut api_client = PreconfApiClient::new(
        api_url,
        chain_id,
        relay_secret_key,
        order_sender,
        access_token,
        api_market_info,
        is_fallback_enabled,
    );
    api_client.login().await;
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
