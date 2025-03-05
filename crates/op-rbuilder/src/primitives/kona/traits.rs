//! [Source](https://github.com/op-rs/kona/blob/a1d8ea603960cb4bd3cc19784f7c3365352f1849/crates/node/rpc/src/traits.rs)

use crate::primitives::kona::{
    ExecutingMessage, SafetyLevel, SupervisorApiClient, CROSS_L2_INBOX_ADDRESS,
};
use alloy_primitives::Log;
use alloy_sol_types::SolEvent;
use async_trait::async_trait;
use core::time::Duration;
use jsonrpsee::{core::ClientError, types::ErrorObjectOwned};
use tokio::time::error::Elapsed;

/// Derived from op-supervisor
const UNKNOWN_CHAIN_MSG: &str = "unknown chain: ";
/// Derived from [op-supervisor](https://github.com/ethereum-optimism/optimism/blob/4ba2eb00eafc3d7de2c8ceb6fd83913a8c0a2c0d/op-supervisor/supervisor/backend/backend.go#L479)
const MINIMUM_SAFETY_MSG: &str = "does not meet the minimum safety";

/// Failures occurring during validation of [`ExecutingMessage`]s.
#[derive(thiserror::Error, Debug)]
pub enum ExecutingMessageValidatorError {
    /// Message does not meet minimum safety level
    #[error("message does not meet min safety level, got: {got}, expected: {expected}")]
    MinimumSafety {
        /// Actual level of the message
        got: SafetyLevel,
        /// Minimum acceptable level that was passed to supervisor
        expected: SafetyLevel,
    },
    /// Invalid chain
    #[error("unsupported chain id: {0}")]
    UnknownChain(u64),
    /// Failure from the [`SupervisorApiClient`] when validating messages.
    #[error("supervisor determined messages are invalid: {0}")]
    RpcClientError(ClientError),

    /// Message validation against the Supervisor took longer than allowed.
    #[error("message validation timed out: {0}")]
    ValidationTimeout(#[from] Elapsed),

    /// Catch-all variant for other supervisor server errors.
    #[error("unexpected error from supervisor: {0}")]
    SupervisorServerError(ErrorObjectOwned),
}

/// Interacts with a Supervisor to validate [`ExecutingMessage`]s.
#[async_trait]
pub trait ExecutingMessageValidator {
    /// The supervisor client type.
    type SupervisorClient: SupervisorApiClient + Send + Sync;

    /// Default duration that message validation is not allowed to exceed.
    const DEFAULT_TIMEOUT: Duration;

    /// Extracts [`ExecutingMessage`]s from the [`Log`] if there are any.
    fn parse_messages(logs: &[Log]) -> impl Iterator<Item = Option<ExecutingMessage>> {
        logs.iter().map(|log| {
            (log.address == CROSS_L2_INBOX_ADDRESS && log.topics().len() == 2)
                .then(|| ExecutingMessage::decode_log_data(&log.data, true).ok())
                .flatten()
        })
    }

    /// Validates a list of [`ExecutingMessage`]s against a Supervisor.
    async fn validate_messages(
        supervisor: &Self::SupervisorClient,
        messages: &[ExecutingMessage],
        safety: SafetyLevel,
        timeout: Option<Duration>,
    ) -> Result<(), ExecutingMessageValidatorError> {
        // Set timeout duration based on input if provided.
        let timeout = timeout.map_or(Self::DEFAULT_TIMEOUT, |t| t);

        // Construct the future to validate all messages using supervisor.
        let fut = async { supervisor.check_messages(messages.to_vec(), safety).await };

        // Await the validation future with timeout.
        match tokio::time::timeout(timeout, fut)
            .await
            .map_err(ExecutingMessageValidatorError::ValidationTimeout)?
        {
            Ok(_) => Ok(()),
            Err(err) => match err {
                ClientError::Call(err) => Err(err.into()),
                _ => Err(ExecutingMessageValidatorError::RpcClientError(err)),
            },
        }
    }
}

impl From<ErrorObjectOwned> for ExecutingMessageValidatorError {
    fn from(value: ErrorObjectOwned) -> Self {
        // Check if it's invalid message call, message example:
        // `failed to check message: failed to check log: unknown chain: 14417`
        if value.message().contains(UNKNOWN_CHAIN_MSG) {
            if let Ok(chain_id) = value
                .message()
                .split(' ')
                .last()
                .expect("message contains chain id")
                .parse::<u64>()
            {
                return Self::UnknownChain(chain_id);
            }
        // Check if it's `does not meet the minimum safety` error, message example:
        // `message {0x4200000000000000000000000000000000000023 4 1 1728507701 901}
        // (safety level: unsafe) does not meet the minimum safety cross-unsafe"`
        } else if value.message().contains(MINIMUM_SAFETY_MSG) {
            let message_safety = if value.message().contains("safety level: safe") {
                SafetyLevel::Safe
            } else if value.message().contains("safety level: local-safe") {
                SafetyLevel::LocalSafe
            } else if value.message().contains("safety level: cross-unsafe") {
                SafetyLevel::CrossUnsafe
            } else if value.message().contains("safety level: unsafe") {
                SafetyLevel::Unsafe
            } else if value.message().contains("safety level: invalid") {
                SafetyLevel::Invalid
            } else {
                // Unexpected level name, return generic error
                return Self::SupervisorServerError(value);
            };
            let expected_safety = if value.message().contains("safety finalized") {
                SafetyLevel::Finalized
            } else if value.message().contains("safety safe") {
                SafetyLevel::Safe
            } else if value.message().contains("safety local-safe") {
                SafetyLevel::LocalSafe
            } else if value.message().contains("safety cross-unsafe") {
                SafetyLevel::CrossUnsafe
            } else if value.message().contains("safety unsafe") {
                SafetyLevel::Unsafe
            } else {
                // Unexpected level name, return generic error
                return Self::SupervisorServerError(value);
            };

            return Self::MinimumSafety {
                expected: expected_safety,
                got: message_safety,
            };
        }
        Self::SupervisorServerError(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN_SAFETY_ERROR: &str = r#"{"code":-32000,"message":"message {0x4200000000000000000000000000000000000023 4 1 1728507701 901} (safety level: unsafe) does not meet the minimum safety cross-unsafe"}"#;
    const INVALID_CHAIN: &str = r#"{"code":-32000,"message":"failed to check message: failed to check log: unknown chain: 14417"}"#;
    const INVALID_LEVEL: &str = r#"{"code":-32000,"message":"message {0x4200000000000000000000000000000000000023 1091637521 4369 0 901} (safety level: invalid) does not meet the minimum safety unsafe"}"#;
    const RANDOM_ERROR: &str = r#"{"code":-32000,"message":"gibberish error"}"#;

    #[test]
    fn test_op_supervisor_error_parsing() {
        assert!(matches!(
            ExecutingMessageValidatorError::from(
                serde_json::from_str::<ErrorObjectOwned>(MIN_SAFETY_ERROR).unwrap()
            ),
            ExecutingMessageValidatorError::MinimumSafety {
                expected: SafetyLevel::CrossUnsafe,
                got: SafetyLevel::Unsafe
            }
        ));

        assert!(matches!(
            ExecutingMessageValidatorError::from(
                serde_json::from_str::<ErrorObjectOwned>(INVALID_CHAIN).unwrap()
            ),
            ExecutingMessageValidatorError::UnknownChain(14417)
        ));

        assert!(matches!(
            ExecutingMessageValidatorError::from(
                serde_json::from_str::<ErrorObjectOwned>(INVALID_LEVEL).unwrap()
            ),
            ExecutingMessageValidatorError::MinimumSafety {
                expected: SafetyLevel::Unsafe,
                got: SafetyLevel::Invalid
            }
        ));

        assert!(matches!(
            ExecutingMessageValidatorError::from(
                serde_json::from_str::<ErrorObjectOwned>(RANDOM_ERROR).unwrap()
            ),
            ExecutingMessageValidatorError::SupervisorServerError(_)
        ));
    }
}
