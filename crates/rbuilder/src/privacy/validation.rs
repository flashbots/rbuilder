use alloy_json_rpc::ErrorPayload;

use crate::validation_api_client::ValidationError;

use super::{error_redactor::ValidationErrorRedactor, REDACTED};

/// Struct representing a redactor that shows all validation errors without modification.
pub struct ShowAllValidationErrorRedactor {}

/// Implementation of ValidationErrorRedactor for ShowAllValidationErrorRedactor.
impl ValidationErrorRedactor for ShowAllValidationErrorRedactor {
    /// Redact method that simply returns the error without any modification.
    fn redact(&self, err: ValidationError) -> ValidationError {
        err
    }
}

/// Struct representing a redactor that avoids exposing any info about txs.
/// Every string with unpredictable content is redacted.
pub struct SecureValidationErrorRedactor {}

impl ValidationErrorRedactor for SecureValidationErrorRedactor {
    fn redact(&self, err: ValidationError) -> ValidationError {
        match err {
            ValidationError::NoValidationNodes => err,
            ValidationError::FailedToSerializeRequest => err,
            ValidationError::NoValidResponseFromValidationNodes => err,
            ValidationError::ValidationFailed(payload) => {
                ValidationError::ValidationFailed(ErrorPayload {
                    code: payload.code,
                    message: REDACTED.to_string(),
                    data: None,
                })
            }
            ValidationError::LocalUsageError(_) => {
                ValidationError::LocalUsageError(Box::from(REDACTED))
            }
        }
    }
}
