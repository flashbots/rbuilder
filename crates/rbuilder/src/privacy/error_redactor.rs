use crate::validation_api_client::ValidationError;

use super::validation::{SecureValidationErrorRedactor, ShowAllValidationErrorRedactor};

/// Entry point struct for all errors.
pub struct ErrorRedactor {
    validation: Box<dyn ValidationErrorRedactor>,
}

impl std::fmt::Debug for ErrorRedactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ErrorRedactor")
    }
}

impl ErrorRedactor {
    pub fn new_show_all() -> Self {
        Self {
            validation: Box::new(ShowAllValidationErrorRedactor {}),
        }
    }

    pub fn new_secure() -> Self {
        Self {
            validation: Box::new(SecureValidationErrorRedactor {}),
        }
    }

    pub fn validation(&self) -> &dyn ValidationErrorRedactor {
        self.validation.as_ref()
    }
}

pub trait ValidationErrorRedactor: Sync + Send {
    fn redact(&self, err: ValidationError) -> ValidationError;
}
