use rbuilder::mev_boost::{RelayClient, SubmitBlockRequest, TestDataGenerator};
use std::str::FromStr;
use url::Url;

// This app is intended to be used for testing against a fake-relay I made (https://github.com/ZanCorDX/fake-relay) without needing all the builder or relay running
#[tokio::main]
async fn main() {
    let relay_url = Url::from_str("http://localhost:8080/").unwrap();
    let mut generator = TestDataGenerator::default();
    let relay = RelayClient::from_url(relay_url.clone(), None, None, None);
    let sub_relay = SubmitBlockRequest::Deneb(generator.create_deneb_submit_block_request());
    relay
        .submit_block(&sub_relay, true, true)
        .await
        .expect("OPS!");
    return;
}
