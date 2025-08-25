use std::io;
use std::time::Duration;

use rbuilder::preconf::preconf_api_client::PreconfApiClient;
use rbuilder::preconf::{PreconfInfo, PreconfReservedInfo, PreconfState};
use rbuilder::utils::{failed_txs_writer, init_reporting_from_preconf_client};
use tokio::sync::{mpsc, watch};
use url::Url;

#[tokio::main]
async fn main() -> io::Result<()> {
	// Configure base URL and JWT via env for testing
	// Example:
	//   PRECONF_BASE_URL="https://hoodi.app.ethgas.com/" PRECONF_ACCESS_TOKEN="eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCJ9.eyJ1c2VyIjp7InVzZXJJZCI6MywiYWRkcmVzcyI6IjB4MGZkNWMwYzMwMGI5MDc1ZDYyNDA2ZGExNDJhYjRiYmFhYzkwOGFlZCIsInJvbGVzIjpbXX0sImFjY2Vzc190eXBlIjoiYWNjZXNzX3Rva2VuIiwiaWF0IjoxNzU1NTkwMDUyLCJleHAiOjE3NTU1OTM2NTJ9.OHCO8W8Y96ghXhcRBkXEqTfZaOcRD8EmGczFAAlWW--KDAm74qgcmKX1v_XtZPx0L1_M3BKBzNIc5pbYzLDDzg" cargo run --bin debug-failed-txs-writer
	let base = std::env::var("PRECONF_BASE_URL").expect("set PRECONF_BASE_URL (e.g. https://hoodi.app.ethgas.com/)");
	let token_opt = std::env::var("PRECONF_ACCESS_TOKEN").ok();

	// Minimal PreconfApiClient just to seed failed_txs_writer globals
	let api_url = Url::parse(&base).expect("invalid PRECONF_BASE_URL");
	let preconf_state = {
		let s = PreconfState::new("0x0000000000000000000000000000000000000000".to_string());
		if let Some(token) = token_opt {
			let mut w = s.access_token.write().await;
			*w = Some(token);
		}
		s
	};

	let (order_sender, _order_receiver) = mpsc::channel(1);
	let (_info_sender, info_receiver) = watch::channel(PreconfInfo { slot: 0, block_number: 0, timestamp: None });
	let (reserved_sender, _reserved_receiver) = watch::channel(PreconfReservedInfo { slot: 0, empty_space: 0, fee_recipient: None });

	let client = PreconfApiClient {
		api_url,
		client: reqwest::Client::new(),
		refresh_token: None,
		access_token_exp: None,
		refresh_token_exp: None,
		order_sender,
		info_receiver,
		reserved_sender,
		state: preconf_state,
		exchange_secret_key: String::new(),
	};

	// Seed failed_txs_writer with HTTP client, base URL, and JWT
	init_reporting_from_preconf_client(&client);

	// Create some sample data
	let new_data = failed_txs_writer::FailedTx {
		slot: 1122382,
		uuid: "b9cc7888-c671-4465-9e4d-6b4ed5ff2192".to_string(),
		tx_hash: "0xCC5C209AAB71B9C59E0EC3A208B3A5E82B068C01B2408C9190909178D70D31C9".to_string(),
		failed_reason: "test-error4".to_string(),
	};

	// Append; this spawns the HTTP post in the background
	failed_txs_writer::append_json(&new_data)?;
	println!("failed transaction is appended.");

	tokio::time::sleep(Duration::from_millis(800)).await;

	Ok(())
}