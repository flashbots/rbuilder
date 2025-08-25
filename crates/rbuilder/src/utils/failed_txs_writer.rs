use crate::preconf::preconf_api_client::PreconfApiClient;
use serde::{Deserialize, Serialize};
use serde_json::{self};
use sqlx::types::chrono::Local;
use std::fs::{self};
use std::io::{self};
use std::path::Path;
use std::sync::OnceLock;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use tokio::sync::RwLock;
use url::Url;
use std::sync::Arc;
use tracing::{debug, error, trace};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FailedTx {
    pub slot: u64,
    pub uuid: String,
    pub tx_hash: String,
    pub failed_reason: String,
}
// 0 OTHER
// 1001 SANCTIONED_ADDRESS - Sender or recipient address is sanctioned (sanctions - ofac)
// 1002 SANCTIONED_CONTRACT - Interaction with a sanctioned contract (sanctions - ofac)
// 1003 SANCTIONED_INTERMEDIARY - Known sanctioned intermediaries - based on emitted events or transaction traces showing interaction with a sanctioned contract in the middle of execution (sanctions - ofac)
// 2001 TAINTED_FUNDS - Transfer of hacked or tainted funds (security)
// 3001 SPAM_CONTRACT - Spam contract (spam)
// 3002 SPAM_SENDER - Spam sender (spam)
// 4001 INVALID_TRANSACTION - Invalid or malformed transaction - incorrect nonce or insufficient gas (likely rejected by reth during simulation) (validation)
// 4002 BUNDLE_REVERT - Transaction would revert and canRevert flag in bundle marked as false (validation)
// 5001 MALICIOUS_MEV - Malicious MEV transaction - sandwich attack, front-run, MEV exploit (mev)
#[derive(Serialize)]
struct RejectEntry {
    uuid: String,
    #[serde(rename = "rejectCode")] 
    reject_code: u32,
    #[serde(rename = "txHashList")] 
    tx_hash_list: String,
    reason: String,
}

#[derive(Serialize)]
struct WebRejectBundleRequest {
    slot: u64,
    rejections: Vec<RejectEntry>,
}

// Globals to share API client state (base URL, HTTP client, and access token) without
// changing call sites in other modules.
static PRECONF_HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
static PRECONF_API_BASE: OnceLock<Url> = OnceLock::new();
static PRECONF_ACCESS_TOKEN: OnceLock<Arc<RwLock<Option<String>>>> = OnceLock::new();

/// Initialize reporting auth and HTTP client state from the existing preconf API client.
/// This allows this module to attach the current JWT automatically when posting.
pub fn init_reporting_from_preconf_client(client: &PreconfApiClient) {
    let _ = PRECONF_HTTP_CLIENT.set(client.client.clone());
    let _ = PRECONF_API_BASE.set(client.api_url.clone());
    let _ = PRECONF_ACCESS_TOKEN.set(Arc::clone(&client.state.access_token));
}

async fn auth_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(lock) = PRECONF_ACCESS_TOKEN.get() {
        let guard = lock.read().await;
        if let Some(token) = guard.as_ref() {
            let authorization_header = format!("Bearer {}", token);
            if let Ok(val) = HeaderValue::from_str(&authorization_header) {
                headers.insert(AUTHORIZATION, val);
            }
        }
    }
    headers
}


fn normalize_tx_hash_upper_no_prefix(tx_hash: &str) -> String {
    tx_hash.trim_start_matches("0x").to_uppercase()
}

async fn submit_rejected_bundle(slot: u64, rejections: Vec<RejectEntry>) -> Result<(), reqwest::Error> {
    
    let client = PRECONF_HTTP_CLIENT
        .get()
        .expect("preconf HTTP client not initialized via init_reporting_from_preconf_client")
        .clone();
    let api_url = PRECONF_API_BASE
        .get()
        .expect("preconf HTTP client not initialized via init_reporting_from_preconf_client")
        .clone();

    let request_url = format!("{}api/v1/builder/bundle/reject/{}", api_url, slot);
    let headers = auth_headers().await;
    let body = WebRejectBundleRequest { slot, rejections };
    if let Ok(json) = serde_json::to_string_pretty(&body) {
        trace!("Reject bundle body:\n{}", json);
    } else {
        trace!("Failed to serialize request body");
    }
    let resp = client.post(request_url).headers(headers).json(&body).send().await?;
    trace!("{:?}", resp);
    resp.error_for_status()?;
    Ok(())
}

async fn send_failed_tx_as_rejection(failed: FailedTx) -> Result<(), reqwest::Error> {
    let entry = RejectEntry {
        uuid: failed.uuid,
        reject_code: 4001,
        tx_hash_list: normalize_tx_hash_upper_no_prefix(&failed.tx_hash),
        reason: failed.failed_reason,
    };
    submit_rejected_bundle(failed.slot, vec![entry]).await
}

pub fn append_json(data: &FailedTx) -> io::Result<()> {
    // Check if the file exists, create it if it doesn't
    // Get the filename based on the current date
    let filename = get_filename();
    let path = Path::new(&filename);
    if !path.exists() {
        // Create a new file with empty array
        fs::write(path, "[]")?;
    }

    // Read the current contents of the file
    let file_contents = fs::read_to_string(path)?;
    let mut json_array: Vec<FailedTx> = serde_json::from_str(&file_contents)?;

    // Append the new data
    json_array.push(data.clone());

    // Write the updated array back to the file
    let updated_contents = serde_json::to_string_pretty(&json_array)?;
    fs::write(path, updated_contents)?;

    if PRECONF_HTTP_CLIENT.get().is_some()
    {
        let data_clone = data.clone();
        tokio::spawn(async move {
            let _ = send_failed_tx_as_rejection(data_clone).await;
        });
    }

    Ok(())
}

fn get_filename() -> String {
    // Get the current date
    let now = Local::now().to_utc();
    let date = now.format("%Y-%m-%d").to_string();
    format!("./failedTxs/rbuilder_failed_txs_{}.json", date)
}