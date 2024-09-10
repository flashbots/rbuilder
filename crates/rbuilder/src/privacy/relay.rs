use crate::mev_boost::{get_reqwest_error_kind, RelayError, SubmitBlockErr};

use super::{error_redactor::RelayErrorRedactor, REDACTED};

/// Struct representing a redactor that shows all relay errors without modification.
pub struct ShowAllRelayErrorRedactor {}

/// Implementation of RelayErrorRedactor for ShowAllRelayErrorRedactor.
impl RelayErrorRedactor for ShowAllRelayErrorRedactor {
    /// Redact method that simply returns the error without any modification.
    fn redact(&self, err: SubmitBlockErr) -> SubmitBlockErr {
        err
    }

    fn redact_sim_error(&self, err: String) -> String {
        err
    }
}

/// Struct representing a redactor that avoids exposing any info about txs.
/// Every string with unpredictable content is redacted.
pub struct SecureRelayErrorRedactor {}

impl RelayErrorRedactor for SecureRelayErrorRedactor {
    fn redact(&self, err: SubmitBlockErr) -> SubmitBlockErr {
        match err {
            SubmitBlockErr::RelayError(err) => SubmitBlockErr::RelayError(redact_relay_error(err)),
            SubmitBlockErr::PayloadAttributesNotKnown => SubmitBlockErr::PayloadAttributesNotKnown,
            SubmitBlockErr::PastSlot => SubmitBlockErr::PastSlot,
            SubmitBlockErr::PayloadDelivered => SubmitBlockErr::PayloadDelivered,
            SubmitBlockErr::BidBelowFloor => SubmitBlockErr::BidBelowFloor,
            SubmitBlockErr::SimError(text) => SubmitBlockErr::SimError(self.redact_sim_error(text)),
            SubmitBlockErr::RPCConversionError(err) => SubmitBlockErr::RPCConversionError(err),
            SubmitBlockErr::RPCSerializationError(_) => {
                SubmitBlockErr::RPCSerializationError(REDACTED.to_string())
            }
            SubmitBlockErr::InvalidHeader => SubmitBlockErr::InvalidHeader,
            SubmitBlockErr::BlockKnown => SubmitBlockErr::BlockKnown,
        }
    }

    fn redact_sim_error(&self, _: String) -> String {
        REDACTED.to_string()
    }
}

/// Redacts sensitive information from RelayError variants.
/// This function ensures that no potentially sensitive data is exposed in error messages.
fn redact_relay_error(err: RelayError) -> RelayError {
    match err {
        RelayError::RequestError(err) => {
            RelayError::RedactedRequestError(get_reqwest_error_kind(&err))
        }
        RelayError::InvalidHeader => RelayError::InvalidHeader,
        RelayError::RelayError(error) => {
            RelayError::RelayError(error.with_message(REDACTED.to_string()))
        }
        RelayError::UnknownRelayError(status, _) => {
            RelayError::UnknownRelayError(status, REDACTED.to_string())
        }
        RelayError::TooManyRequests => RelayError::TooManyRequests,
        RelayError::ConnectionError => RelayError::ConnectionError,
        RelayError::InternalError => RelayError::InternalError,
        RelayError::RedactedRequestError(kind) => RelayError::RedactedRequestError(kind),
    }
}
