use crate::preconf::{
    assign_preconf_ordering, convert_str_to_address, convert_timestamp_ns, eth_to_wei,
    string_to_uuid, PreconfBundleType, PreconfError, PreconfHealthStatus, PreconfInfo,
    PreconfReservedInfo, PreconfState,
};
use crate::primitives::{
    Bundle, BundleReplacementData, BundleReplacementKey, BundleVersion, Metadata, Order,
    TransactionSignedEcRecoveredWithBlobs,
};
use alloy_dyn_abi::eip712::TypedData;
use alloy_primitives::{hex, keccak256, Bytes, B256};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use reqwest::Response;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, trace, warn};
use url::Url;

#[derive(Debug, Deserialize)]
struct ApiResponse {
    success: bool,
    data: ApiData,
}

impl std::fmt::Display for ApiResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ApiResponse {{ success: {} }}", self.success)
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ApiData {
    InclusionPreconfMarket(InclusionPreconfMarket),
    InclusionPreconfMarkets(InclusionPreconfMarkets),
    PreconfBundles(PreconfBundles),
    Login(LoginResponse),
    Verify(VerifyResponse),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginResponse {
    // status: String,
    eip712_message: String,
    nonce_hash: String,
}

// #[derive(Debug, Deserialize)]
// #[serde(rename_all = "camelCase")]
// struct EIP712Message {
//     types: EIP712Types,
//     primary_type: String,
//     message: TypedMessage,
//     domain: Domain,
// }
//
// #[derive(Debug, Deserialize)]
// struct EIP712Types {
//     #[serde(rename = "EIP712Domain")]
//     eip712_domain: Vec<Field>,
//     data: Vec<Field>,
// }
//
// #[derive(Debug, Deserialize)]
// struct Field {
//     name: String,
//     #[serde(rename = "type")]
//     field_type: String,
// }
//
// #[derive(Debug, Deserialize)]
// struct TypedMessage {
//     hash: String,
//     message: String,
//     domain: String,
// }
//
// #[derive(Debug, Deserialize)]
// #[serde(rename_all = "camelCase")]
// struct Domain {
//     name: String,
//     version: String,
//     chain_id: u64,
//     verifying_contract: String,
// }

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VerifyResponse {
    // user: User,
    access_token: AccessToken,
}

// #[derive(Debug, Deserialize)]
// #[serde(rename_all = "camelCase")]
// pub struct User {
//     user_id: u64,
//     address: String,
//     user_type: u32,
//     accounts: Vec<Account>,
//     #[serde(default)]
//     status: Option<u32>,
//     #[serde(default)]
//     user_class: Option<u32>,
//     #[serde(default)]
//     display_name: Option<String>,
// }

// #[derive(Debug, Deserialize)]
// #[serde(rename_all = "camelCase")]
// pub struct Account {
//     #[serde(rename = "type")]
//     account_type: u32,
//     account_id: u64,
//     #[serde(default)]
//     user_id: Option<u64>,
//     #[serde(default)]
//     name: Option<String>,
//     #[serde(default)]
//     status: Option<u32>,
//     #[serde(default)]
//     update_date: Option<u64>,
// }

#[derive(Deserialize, Debug)]
pub struct AccessToken {
    data: TokenData,
    token: String,
}

#[derive(Deserialize, Debug)]
pub struct TokenData {
    // header: Header,
    payload: Payload,
}

// #[derive(Deserialize, Debug)]
// pub struct Header {
//     alg: String,
//     typ: String,
// }

#[derive(Deserialize, Debug)]
pub struct Payload {
    // user: PayloadUser,
    // access_type: String,
    // iat: i64,
    exp: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadUser {
    pub user_id: u64,
    pub address: String,
    pub roles: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct InclusionPreconfMarkets {
    markets: Vec<PreconfMarketInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
struct InclusionPreconfMarket {
    market: PreconfMarketInfo,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreconfMarketInfo {
    // market_id: u64,
    slot: u64,
    // proposer_account_id: u64,
    // instrument_id: String,
    // name: String,
    // quantity_step: String,
    // min_quantity: String,
    // max_quantity: String,
    // price_step: String,
    // min_price: String,
    // max_price: String,
    // collateral_per_gas: String,
    // best_bid: String,
    // best_ask: String,
    // direction: bool,
    // price: String,
    // mid_price: String,
    // status: u8,
    // maturity_time: u64,
    trx_submit_time: u64,
    // block_time: u64,
    // finality_time: u64,
    // update_date: u64,
    // total_gas: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreconfBundles {
    slot: u64,
    bundles: Vec<PreconfBundle>,
    empty_space: i64,
    fee_recipient: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreconfBundle {
    txs: Vec<PreconfTx>,
    replacement_uuid: String,
    bid_price: String,
    ordering: Option<i8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bundle_type: Option<PreconfBundleType>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreconfTx {
    tx: String,
    tx_hash: String,
    can_revert: bool,
}

#[derive(Debug)]
pub struct PreconfApiClient {
    pub api_url: Url,
    pub client: reqwest::Client,
    pub refresh_token: Option<String>,
    pub access_token_exp: Option<i64>,
    pub refresh_token_exp: Option<i64>,
    pub order_sender: mpsc::Sender<Order>,
    pub info_receiver: watch::Receiver<PreconfInfo>,
    pub reserved_sender: watch::Sender<PreconfReservedInfo>,
    pub state: PreconfState,
    pub relay_secret_key: String,
}

impl PreconfApiClient {
    pub async fn get_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let guard = self.state.access_token.read().await;
        let access_token = guard.clone();
        if access_token.is_some() {
            let token = access_token.clone().unwrap();
            let authorization_header = format!("Bearer {}", token);
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(authorization_header.as_str()).unwrap(),
            );
        }
        headers.insert(USER_AGENT, HeaderValue::from_str("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/114.0.0.0 Safari/537.36").unwrap());
        headers
    }

    pub async fn re_login(&mut self) -> bool {
        let mut logged_in = false;
        let mut retry_count = 0;
        loop {
            if self.login().await {
                logged_in = true;
                info!("ETHGas re-logged in.");
                break;
            }
            retry_count += 1;
            if retry_count % 5 == 0 {
                error!("Failed to login ETHGas, continue to retry login...");
            }
        }
        logged_in
    }

    pub async fn login(&mut self) -> bool {
        let mut is_logged_in = false;

        let secret_key_bytes = hex::decode(self.relay_secret_key.as_str())
            .expect("Failed to decode secret key for preconf login");
        let signer: PrivateKeySigner = PrivateKeySigner::from_slice(secret_key_bytes.as_ref())
            .expect("Failed to create signer from secret key for preconf login");
        let address = format!("0x{:x}", signer.address());
        let login_headers = self.get_headers().await;

        let login_url = format!("{}api/v1/user/login", self.api_url);
        let login_response = match self
            .client
            .post(&login_url)
            .headers(login_headers)
            .form(&[("addr", address.clone())])
            .send()
            .await
        {
            Ok(response) => {
                info!("Received the login response from ETHGas.");
                response
            },
            Err(e) => {
                let mut guard = self.state.health_status.write().await;
                *guard = PreconfHealthStatus::ServerFailed;
                error!("Failed to login ETHGas: {}", e);
                return is_logged_in;
            }
        };

        if login_response.status().is_success() {
            if let ApiData::Login(login_resp) = login_response
                .json::<ApiResponse>()
                .await
                .expect("Failed to decode ETHGas login response")
                .data
            {
                if !login_resp.nonce_hash.is_empty() && !login_resp.eip712_message.is_empty() {
                    let eip712_msg: TypedData = serde_json::from_str(&login_resp.eip712_message)
                        .expect("Failed to parse EIP712 message into typed data");
                    let signature = signer
                        .sign_dynamic_typed_data(&eip712_msg)
                        .await
                        .expect("Failed to sign EIP712 message");
                    let signature_hex_str = format!("0x{}", hex::encode(signature.as_bytes()));
                    let verify_headers = self.get_headers().await;
                    // debug!("Generated signature: {}", signature_hex_str);

                    // Send the signature back to complete verification
                    let verify_response = match self
                        .client
                        .post(format!("{}api/v1/user/login/verify", self.api_url))
                        .headers(verify_headers)
                        .form(&[
                            ("addr", address.clone()),
                            ("signature", signature_hex_str),
                            ("nonceHash", login_resp.nonce_hash),
                        ])
                        .send()
                        .await
                    {
                        Ok(response) => {
                            info!("Received the verify response from ETHGas.");
                            response
                        },
                        Err(e) => {
                            error!("Failed to verify signature: {}", e);
                            return is_logged_in;
                        }
                    };

                    if verify_response.status().is_success() {
                        let (refresh_token, refresh_token_exp) =
                            self.extract_refresh_token(&verify_response);
                        trace!("refresh token: {:?}", refresh_token);
                        if let ApiData::Verify(verify_resp) = verify_response
                            .json::<ApiResponse>()
                            .await
                            .expect("Failed to decode verification response")
                            .data
                        {
                            let access_token = verify_resp.access_token.token;
                            let access_token_exp = verify_resp.access_token.data.payload.exp;
                            trace!("JWT access token: {:?}", access_token);
                            trace!("Expired at: {:?}", access_token_exp);
                            let mut access_token_writer = self.state.access_token.write().await;
                            *access_token_writer = Some(access_token);
                            self.refresh_token = refresh_token;
                            self.access_token_exp = Some(access_token_exp);
                            self.refresh_token_exp = refresh_token_exp;
                            is_logged_in = true;
                            info!("ETHGas login successful.");
                        }
                    } else {
                        error!(
                            "Failed to verify ETHGas login signature, Err: {:?}",
                            verify_response.text().await
                        );
                    }
                }
            }
        }
        is_logged_in
    }

    fn extract_refresh_token(&self, response: &Response) -> (Option<String>, Option<i64>) {
        // Extract the cookies from the response
        let cookies: HashMap<String, String> = response
            .cookies()
            .map(|cookie| (cookie.name().to_string(), cookie.value().to_string()))
            .collect();
        // 7 days
        let refresh_token_exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Failed to get current time")
            .as_secs() as i64
            + Duration::from_secs(7 * 24 * 60 * 60).as_secs() as i64;

        // Look for the x_auth_refresh_token cookie and return its value
        (
            cookies.get("x_auth_refresh_token").cloned(),
            Some(refresh_token_exp),
        )
    }

    fn is_token_expired(&self, target_ts: i64) -> bool {
        // Get the current time in UTC
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Failed to get current time")
            .as_secs() as i64;
        current_time >= target_ts
    }

    pub async fn refresh_access(&mut self) {
        if self.refresh_token_exp.is_some() {
            let refresh_expiry: i64 = self.refresh_token_exp.unwrap() - (24 * 60 * 60);
            if self.is_token_expired(refresh_expiry) {
                let logged_in = self.login().await;
                if !logged_in {
                    error!("Failed to refresh access token due to failed login.");
                }
            } else {
                let start = Instant::now();
                debug!("refreshing access token...");
                self.refresh_access_token().await;
                let elapsed = start.elapsed();
                debug!("refresh access token elapsed: {:?}", elapsed);
            }
        } else {
            error!(
                "Failed to refresh access token due to missing refresh token expiry, Err: {:?}",
                self.refresh_token_exp
            );
        }
    }

    async fn refresh_access_token(&mut self) {
        if self.access_token_exp.is_some() {
            // Get the target timestamp (access token expiration - 20 minutes)
            let access_expiry: i64 = self.access_token_exp.unwrap() - (20 * 60);
            if !self.is_token_expired(access_expiry) {
                return;
            }
        }
        let refresh_headers = self.get_headers().await;
        let refresh_url = format!("{}api/v1/user/login/refresh", self.api_url);
        let refresh_token = self.refresh_token.clone().unwrap();
        let refresh_response = self
            .client
            .post(&refresh_url)
            .headers(refresh_headers)
            .form(&[("refreshToken", refresh_token.as_str())])
            .send()
            .await
            .expect("Failed to refresh access token");

        if refresh_response.status().is_success() {
            if let ApiData::Verify(refresh_resp) = refresh_response
                .json::<ApiResponse>()
                .await
                .expect("Failed to decode refresh response")
                .data
            {
                debug!(
                    "new JWT access token: {:?}",
                    refresh_resp.access_token.token
                );
                let mut access_token_writer = self.state.access_token.write().await;
                *access_token_writer = Some(refresh_resp.access_token.token);
                self.access_token_exp = Some(refresh_resp.access_token.data.payload.exp);
            }
        } else {
            error!(
                "Failed to refresh access token, Err: {:?}",
                refresh_response.text().await
            );
        }
    }
}

impl PreconfApiClient {
    async fn clean_market_info(&self, slot: u64) {
        let reader = self.state.market_info.read().await;
        let mut do_clean = false;
        if reader.len() > 64 {
            do_clean = true;
        }
        drop(reader);
        if do_clean {
            let til = slot - 1;
            let mut writer = self.state.market_info.write().await;
            for key in 1..=til {
                writer.remove(&key);
            }
        }
    }

    pub async fn get_inclusion_preconf_market_expiry(&self, slot: u64) -> Option<OffsetDateTime> {
        debug!("get market expiry on slot={}", slot);
        // clean market info
        self.clean_market_info(slot).await;
        let reader = self.state.market_info.read().await;
        if reader.contains_key(&slot) {
            reader.get(&slot).cloned()
        } else {
            // Try to get the market info from the map or load it if it's not present.
            let mut market_t = self.get_active_inclusion_preconf_market_info(slot).await;
            if market_t.is_none() {
                market_t = self.get_inclusion_preconf_market_info(slot).await;
            }
            if market_t.is_none() {
                warn!("cannot get the market info from the map or load it when it's not present on slot({})", slot);
            }
            market_t
        }
    }

    async fn get_inclusion_preconf_market_info(&self, curr_slot: u64) -> Option<OffsetDateTime> {
        let url = format!(
            "{}api/v1/p/inclusion-preconf/market?slot={}",
            self.api_url,
            curr_slot.clone()
        );
        match self.client.get(&url).send().await {
            Ok(r) => {
                let market_resp = r.text().await.unwrap();
                debug!(
                    "Raw inclusion preconf market api response: {:?}",
                    market_resp
                );
                match serde_json::from_str::<ApiResponse>(&market_resp) {
                    Ok(api_resp) => {
                        let mut market_expiry: Option<OffsetDateTime> = None;
                        if !api_resp.success {
                            error!(
                                "Inclusion preconf market api response is failed: {}",
                                api_resp
                            );
                        } else if let ApiData::InclusionPreconfMarket(info) = api_resp.data {
                            if curr_slot == info.market.slot {
                                let datetime = convert_timestamp_ns(info.market.trx_submit_time);
                                market_expiry = Some(datetime);
                            }
                            let market_info = Arc::clone(&self.state.market_info);
                            tokio::spawn(async move {
                                let mut writer = market_info.write().await;
                                let datetime = convert_timestamp_ns(info.market.trx_submit_time);
                                writer.insert(info.market.slot, datetime);
                            });
                        }
                        market_expiry
                    }
                    Err(err) => {
                        error!("Failed to parse market info api response: {}", err);
                        None
                    }
                }
            }
            Err(err) => {
                let mut guard = self.state.health_status.write().await;
                *guard = PreconfHealthStatus::ServerFailed;
                error!("Cannot fetch market info from exchange: {}", err);
                None
            }
        }
    }

    async fn get_active_inclusion_preconf_market_info(
        &self,
        curr_slot: u64,
    ) -> Option<OffsetDateTime> {
        let url = format!("{}api/v1/p/inclusion-preconf/markets", self.api_url);
        match self.client.get(&url).send().await {
            Ok(r) => {
                let market_resp = r.text().await.unwrap();
                debug!(
                    "Raw active inclusion preconf markets api response: {:?}",
                    market_resp
                );
                match serde_json::from_str::<ApiResponse>(&market_resp) {
                    Ok(api_resp) => {
                        let mut market_expiry: Option<OffsetDateTime> = None;
                        if !api_resp.success {
                            error!(
                                "Active inclusion preconf markets api response is failed: {}",
                                api_resp
                            );
                        } else if let ApiData::InclusionPreconfMarkets(market_data) = api_resp.data
                        {
                            let len = market_data.markets.len();
                            if len > 0 {
                                for i in 0..len {
                                    let market = &market_data.markets[i];
                                    if curr_slot == market.slot {
                                        let datetime = convert_timestamp_ns(market.trx_submit_time);
                                        market_expiry = Some(datetime);
                                        break;
                                    }
                                }
                                let market_info = Arc::clone(&self.state.market_info);
                                tokio::spawn(async move {
                                    let mut writer = market_info.write().await;
                                    for market in market_data.markets {
                                        let datetime = convert_timestamp_ns(market.trx_submit_time);
                                        writer.insert(market.slot, datetime);
                                    }
                                });
                            }
                        }
                        market_expiry
                    }
                    Err(err) => {
                        error!("Failed to parse market info api response: {}", err);
                        None
                    }
                }
            }
            Err(err) => {
                let mut guard = self.state.health_status.write().await;
                *guard = PreconfHealthStatus::ServerFailed;
                error!("Cannot fetch market info from exchange: {}", err);
                None
            }
        }
    }

    pub async fn fetch_inclusion_preconfs(&self, preconf_info: PreconfInfo) {
        info!("fetch preconfs: {:?}", preconf_info);
        let url = format!(
            "{}api/v1/slot/bundles?slot={}",
            self.api_url, preconf_info.slot
        );
        let get_headers = self.get_headers().await;
        match self.client.get(&url).headers(get_headers).send().await {
            Ok(r) => {
                let preconf_resp = r.text().await.unwrap();
                debug!("Raw fetch preconfs api response: {:?}", preconf_resp);
                match serde_json::from_str::<ApiResponse>(&preconf_resp) {
                    Ok(resp) => {
                        if !resp.success {
                            error!("Failed to fetch inclusion preconf from server: {}", resp);
                            let gas_info = PreconfReservedInfo {
                                slot: preconf_info.slot.clone(),
                                empty_space: 0,
                                fee_recipient: Some(
                                    self.state.get_fallback_fee_recipient().clone(),
                                ),
                            };
                            self.reserved_sender.send(gas_info).unwrap();
                        } else {
                            if let ApiData::PreconfBundles(bundle_response) = resp.data {
                                if bundle_response.bundles.is_empty() {
                                    debug!(
                                        "received empty preconf bundle response: {:?}",
                                        bundle_response
                                    );
                                } else {
                                    debug!(
                                        "received preconf bundle response: {:?}",
                                        bundle_response
                                    );
                                    let order_sender = self.order_sender.clone();
                                    tokio::spawn(async move {
                                        for bundle in bundle_response.bundles {
                                            if bundle.txs.is_empty() {
                                                debug!("received preconf bundle contains zero transaction.");
                                            } else {
                                                match generate_order_from_api_preconf(
                                                    preconf_info.block_number,
                                                    preconf_info.timestamp.unwrap(),
                                                    bundle,
                                                ) {
                                                    Ok(preconf_order) => {
                                                        if preconf_order.has_blobs() {
                                                            debug!(
                                                                "preconf order(id={}) has blobs",
                                                                preconf_order.id().to_string()
                                                            );
                                                        } else {
                                                            debug!("preconf order(id={}) do not have blobs", preconf_order.id().to_string());
                                                        }
                                                        if order_sender
                                                            .send(preconf_order)
                                                            .await
                                                            .is_err()
                                                        {
                                                            error!("receiver closed");
                                                        }
                                                    }
                                                    Err(err) => {
                                                        error!("Failed to generate order from preconf: {}", err);
                                                    }
                                                }
                                            }
                                        }
                                    });
                                };
                                let fee_recipient =
                                    bundle_response.fee_recipient.map(convert_str_to_address);
                                let gas_info = PreconfReservedInfo {
                                    slot: bundle_response.slot.clone(),
                                    empty_space: bundle_response.empty_space.clone(),
                                    fee_recipient,
                                };
                                self.reserved_sender.send(gas_info).unwrap();
                            }
                        }
                    }
                    Err(err) => {
                        error!("Failed to fetch preconf request: {}", err);
                        let gas_info = PreconfReservedInfo {
                            slot: preconf_info.slot.clone(),
                            empty_space: 0,
                            fee_recipient: Some(self.state.get_fallback_fee_recipient().clone()),
                        };
                        self.reserved_sender.send(gas_info).unwrap();
                        return; // Exit if JSON parsing fails
                    }
                }
            }
            Err(err) => {
                let mut guard = self.state.health_status.write().await;
                *guard = PreconfHealthStatus::ServerFailed;
                error!("cannot fetch preconf requests from preconf server, {}", err);
            }
        }
    }
}

fn generate_order_from_api_preconf(
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
        key: BundleReplacementKey::new(bundle_uuid, Some(signer.unwrap())),
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
                version: BundleVersion::V1,
                block: Some(block),
                min_timestamp: Some(timestamp),
                max_timestamp: None,
                txs: trxs,
                reverting_tx_hashes,
                dropping_tx_hashes: vec![],
                hash: bundle_hash,
                uuid: bundle_uuid,
                replacement_data: Some(replacement_data),
                signer,
                metadata,
                refund: None,
            }))
        }
        Err(e) => Err(PreconfError::PreconfConvertError(format!(
            "Cannot generate order from bundle: {}",
            e
        ))),
    }
}
