use std::sync::Arc;

use crate::{mev_boost::SubmitBlockErr, validation_api_client::ValidationError};

use super::{
    relay::{SecureRelayErrorRedactor, ShowAllRelayErrorRedactor},
    validation::{SecureValidationErrorRedactor, ShowAllValidationErrorRedactor},
};

/// Entry point struct for all errors.
pub struct ErrorRedactor {
    validation: Arc<dyn ValidationErrorRedactor>,
    relay: Arc<dyn RelayErrorRedactor>,
}

impl std::fmt::Debug for ErrorRedactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ErrorRedactor")
    }
}

impl ErrorRedactor {
    pub fn new_show_all() -> Self {
        Self {
            validation: Arc::new(ShowAllValidationErrorRedactor {}),
            relay: Arc::new(ShowAllRelayErrorRedactor {}),
        }
    }

    pub fn new_secure() -> Self {
        Self {
            validation: Arc::new(SecureValidationErrorRedactor {}),
            relay: Arc::new(SecureRelayErrorRedactor {}),
        }
    }

    pub fn validation(&self) -> Arc<dyn ValidationErrorRedactor> {
        self.validation.clone()
    }

    pub fn relay(&self) -> Arc<dyn RelayErrorRedactor> {
        self.relay.clone()
    }
}

pub trait ValidationErrorRedactor: Sync + Send {
    fn redact(&self, err: ValidationError) -> ValidationError;
}

pub trait RelayErrorRedactor: Sync + Send {
    fn redact(&self, err: SubmitBlockErr) -> SubmitBlockErr;
    fn redact_sim_error(&self, err: String) -> String;
}
