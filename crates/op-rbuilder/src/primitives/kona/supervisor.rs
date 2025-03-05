//! This is our custom implementation of validator struct

use crate::primitives::kona::ExecutingMessageValidator;
use jsonrpsee::http_client::HttpClient;
use std::time::Duration;

pub struct SupervisorValidator;

impl ExecutingMessageValidator for SupervisorValidator {
    type SupervisorClient = HttpClient;
    const DEFAULT_TIMEOUT: Duration = Duration::from_millis(100);
}
