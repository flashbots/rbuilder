use crate::{
    flashbots_config::default_blocks_processor_max_request_size_bytes,
    signed_http_client::SignedHttpClient,
};
use alloy_signer_local::PrivateKeySigner;
use jsonrpsee::core::client::ClientT;
use tokio::runtime::Runtime;
use tracing::error;

use super::best_true_value_pusher::{Backend, BuiltBlockInfo};

const REPORT_BEST_TRUE_VALUE_METHOD: &str = "flashbots_reportBestTrueValue";

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Unable to build http client {0}")]
    BuildHttpClient(#[from] jsonrpsee::core::Error),
    #[error("Tokio runtime creation error {0}")]
    TokioRuntimeCreation(#[from] std::io::Error),
}

/// Backend for BestTrueValuePusher that sends signed JSON RPC to BlocksProcessor service.
pub struct BlocksProcessorBackend {
    url: String,
    signer: PrivateKeySigner,
    /// A `current_thread` runtime for executing operations on the
    /// asynchronous client in a blocking manner. For more info: https://tokio.rs/tokio/topics/bridging
    runtime: Runtime,
    max_concurrent_requests: usize,
}

impl BlocksProcessorBackend {
    pub fn new(
        url: String,
        signer: PrivateKeySigner,
        max_concurrent_requests: usize,
    ) -> Result<Self, Error> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            url,
            signer,
            runtime,
            max_concurrent_requests,
        })
    }
}

impl Backend for BlocksProcessorBackend {
    type Connection = SignedHttpClient;
    type BackendError = Error;

    fn connect(&self) -> Result<Self::Connection, Self::BackendError> {
        Ok(crate::signed_http_client::create_client(
            &self.url,
            self.signer.clone(),
            // we use default here because request is small
            default_blocks_processor_max_request_size_bytes(),
            self.max_concurrent_requests,
        )?)
    }

    fn publish(
        &self,
        connection: &mut Self::Connection,
        best_true_value: &BuiltBlockInfo,
    ) -> Result<(), Self::BackendError> {
        let params = [best_true_value];
        Ok(self
            .runtime
            .block_on(connection.request(REPORT_BEST_TRUE_VALUE_METHOD, params))?)
    }
}
