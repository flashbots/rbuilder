use crate::flashbots_signer::{FlashbotsSigner, FlashbotsSignerLayer};
use alloy_signer_local::PrivateKeySigner;
use jsonrpsee::http_client::transport::Error as JsonError;
use jsonrpsee::http_client::HttpClientBuilder;
use jsonrpsee::http_client::{transport::HttpBackend, HttpClient};
use tower::ServiceBuilder;
type MapErrorFn = fn(Box<dyn std::error::Error + Send + Sync>) -> JsonError;

const fn map_error(err: Box<dyn std::error::Error + Send + Sync>) -> JsonError {
    JsonError::Http(err)
}

pub type SignedHttpClient =
    HttpClient<tower::util::MapErr<FlashbotsSigner<PrivateKeySigner, HttpBackend>, MapErrorFn>>;

pub fn create_client(
    url: &str,
    signer: PrivateKeySigner,
    max_request_size: u32,
    max_concurrent_requests: usize,
) -> Result<SignedHttpClient, jsonrpsee::core::Error> {
    let signing_middleware = FlashbotsSignerLayer::new(signer);
    let service_builder = ServiceBuilder::new()
        // Coerce to function pointer and remove the + 'static added to the closure
        .map_err(map_error as MapErrorFn)
        .layer(signing_middleware);
    let client = HttpClientBuilder::default()
        .max_request_size(max_request_size)
        .max_concurrent_requests(max_concurrent_requests)
        .set_middleware(service_builder)
        .build(url)?;
    Ok(client)
}
