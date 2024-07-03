use alloy_json_rpc::{ErrorPayload, RpcError};
use std::sync::Arc;

use crate::{
    mev_boost::SubmitBlockRequest,
    telemetry::add_block_validation_time,
    utils::{http_provider, BoxedProvider},
    validation_api_client::ValdationError::UnableToValidate,
};
use alloy_primitives::B256;
use alloy_provider::Provider;
use serde::Serialize;
use serde_with::{serde_as, DisplayFromStr};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info_span, warn};

#[serde_as]
#[derive(Debug, Serialize, Clone)]
struct ValidRequest {
    #[serde(flatten)]
    req: SubmitBlockRequest,
    #[serde_as(as = "DisplayFromStr")]
    registered_gas_limit: u64,
    withdrawals_root: B256,
    parent_beacon_block_root: Option<B256>,
}

#[derive(Debug, Clone)]
pub struct ValidationAPIClient {
    providers: Vec<Arc<BoxedProvider>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ValdationError {
    #[error("Unable to validate block: {0}")]
    UnableToValidate(String),
    #[error("Validation failed")]
    ValidationFailed(ErrorPayload),
}

impl ValidationAPIClient {
    pub fn new(urls: &[&str]) -> eyre::Result<Self> {
        let mut providers = Vec::new();
        for url in urls {
            providers.push(Arc::new(http_provider(url.parse()?)));
        }
        Ok(Self { providers })
    }

    pub async fn validate_block(
        &self,
        req: &SubmitBlockRequest,
        registered_gas_limit: u64,
        withdrawals_root: B256,
        parent_beacon_block_root: Option<B256>,
        cancellation_token: CancellationToken,
    ) -> Result<(), ValdationError> {
        let start = std::time::Instant::now();
        if self.providers.is_empty() {
            return Err(UnableToValidate("No validation nodes".to_string()));
        }

        let method = match req {
            SubmitBlockRequest::Capella(_) => "flashbots_validateBuilderSubmissionV2",
            SubmitBlockRequest::Deneb(_) => "flashbots_validateBuilderSubmissionV3",
        };
        let request = ValidRequest {
            req: req.clone(),
            registered_gas_limit,
            withdrawals_root,
            parent_beacon_block_root,
        };

        // cancellation will make sure that all spawned tasks will terminate when function call is finished
        let cancellation_token = cancellation_token.child_token();
        let _cancel_guard = cancellation_token.clone().drop_guard();

        let (result_sender, mut result_receiver) = mpsc::channel(self.providers.len());

        for (i, provider) in self.providers.iter().enumerate() {
            let cancellation_token = cancellation_token.clone();
            let result_sender = result_sender.clone();
            let provider = provider.clone();
            let request = request.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                    }
                    result = provider.raw_request::<_, ()>(std::borrow::Cow::Borrowed(method), vec![request]) => {
                        _ = result_sender.send((i, result)).await;
                    }
                }
            });
        }

        // We get the first response but if it is not Ok or ErrorResp we try the next one.
        while let Some((idx, result)) = result_receiver.recv().await {
            let span = info_span!("block_validation", validation_node_idx = idx);
            let _span_guard = span.enter();
            match result {
                Ok(()) => {
                    // this means that block passed validation
                    add_block_validation_time(start.elapsed());
                    return Ok(());
                }
                Err(RpcError::ErrorResp(err)) => {
                    if is_error_critical(&err.message) {
                        error!(err = ?err, "Validation node returned error");
                        // this should mean that block did not pass validation
                        add_block_validation_time(start.elapsed());
                        return Err(ValdationError::ValidationFailed(err));
                    } else {
                        warn!(err = ?err, "Unable to validate block");
                    }
                }
                Err(RpcError::SerError(err)) => {
                    error!(err = ?err, "Serialization error");
                    // we will not recover from this error so no point for waiting for other responses
                    return Err(UnableToValidate("Failed to serialize request".to_string()));
                }
                Err(RpcError::DeserError { err, text }) => {
                    if !(text.contains("504 Gateway Time-out") || text.contains("502 Bad Gateway"))
                    {
                        warn!(err = ?err, "Deserialization error");
                    }
                    // usually this means something wrong with the node, so we wait for other responses
                }
                Err(RpcError::Transport(_)) => {
                    warn!("Failed to send request to validation node");
                    // usually this means something wrong with the node or connection, so we wait for other responses
                }
                Err(RpcError::NullResp) => {
                    warn!("Validation node returned null response");
                    // usually this means something wrong with the node, so we wait for other responses
                }
                Err(RpcError::UnsupportedFeature(err)) => {
                    warn!(err = ?err, "Validation node does not support this feature");
                    // usually this means something wrong with the node, so we wait for other responses
                }
                Err(RpcError::LocalUsageError(err)) => {
                    error!(err = ?err, "Local usage error");
                    // we will not recover from this error so no point for waiting for other responses
                    return Err(UnableToValidate(format!("Local usage error: {:?}", err)));
                }
            }
        }

        // if we did not return by this point we did not validate block
        Err(UnableToValidate(
            "Failed to validate block, no valid responses from validation nodes".to_string(),
        ))
    }
}

// Some errors from validation node are not actually errors.
fn is_error_critical(msg: &str) -> bool {
    if msg.contains("unknown ancestor") {
        return false;
    }

    if msg.contains("missing trie node") {
        return false;
    }

    if msg.contains("request timeout hit") {
        return false;
    }

    true
}
