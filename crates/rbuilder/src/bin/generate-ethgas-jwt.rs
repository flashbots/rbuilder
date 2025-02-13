use std::sync::{Arc, Mutex};
use rbuilder::preconf::preconf_api_client::SimplePreconfApiClient;
use url::Url;

#[tokio::main]
async fn main() -> Result<(), reqwest::Error> {
    let start_ts = std::time::Instant::now();
    // please use your account private key
    // example: let private_key: Option<String> = Some(String::from("<private key>"));
    let private_key: Option<String> = None;
    if private_key.is_some() {
        let api_domain = "<EthGas API domain>";
        let chain_id = "<Chain ID in hexadecimal format>";

        let mut api_client: SimplePreconfApiClient = SimplePreconfApiClient::new(Url::parse(api_domain).unwrap(), chain_id.to_string(), private_key.unwrap().to_string(), Arc::new(Mutex::new(None)));
        println!("logging in...");
        api_client.login().await;
        println!("JWT access token: {:?}, expired at: {:?}", api_client.access_token.lock().unwrap().clone().unwrap(), api_client.access_token_exp.clone());
        println!("refresh token: {:?}, expired at: {:?}", api_client.refresh_token.clone(), api_client.refresh_token_exp.clone());
        println!("refreshing access token...");
        api_client.refresh_access().await;
        println!("new JWT access token: {:?}, expired at: {:?}", api_client.access_token.lock().unwrap().clone().unwrap(), api_client.access_token_exp.clone());
        println!("refresh token: {:?}, expired at: {:?}", api_client.refresh_token.clone(), api_client.refresh_token_exp.clone());

        println!("Time spent: {:?}", start_ts.elapsed());
    }

    Ok(())
}