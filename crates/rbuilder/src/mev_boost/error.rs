use reqwest::{self, StatusCode};
use serde::{Deserialize, Serialize};
use std::fmt::{self, Debug, Display, Formatter};
use thiserror::Error;

#[derive(Error)]
pub enum RelayError {
    #[error("Request error: {0}")]
    RequestError(#[from] RedactableReqwestError),
    #[error("Header error")]
    InvalidHeader,
    #[error("Relay error: {0}")]
    RelayError(#[from] RedactableRelayErrorResponse),

    #[cfg_attr(
        not(feature = "redact-sensitive"),
        error("Unknown relay response, status: {0}, body: {1}")
    )]
    #[cfg_attr(
        feature = "redact-sensitive",
        error("Unknown relay response, status: {0}, body: [REDACTED]")
    )]
    UnknownRelayError(StatusCode, String),
    #[error("Too many requests")]
    TooManyRequests,
    #[error("Connection error")]
    ConnectionError,
    #[error("Internal Error")]
    InternalError,
}

impl Debug for RelayError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl From<reqwest::Error> for RelayError {
    fn from(err: reqwest::Error) -> Self {
        RelayError::RequestError(RedactableReqwestError(err))
    }
}

#[derive(Error)]
pub struct RedactableReqwestError(reqwest::Error);

impl From<reqwest::Error> for RedactableReqwestError {
    fn from(err: reqwest::Error) -> Self {
        RedactableReqwestError(err)
    }
}

impl Display for RedactableReqwestError {
    #[cfg(not(feature = "redact-sensitive"))]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }

    #[cfg(feature = "redact-sensitive")]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.0.is_builder() {
            write!(f, "Redacted Reqwest Error: Builder")
        } else if self.0.is_request() {
            write!(f, "Redacted Reqwest Error: Request")
        } else if self.0.is_redirect() {
            write!(f, "Redacted Reqwest Error: Redirect")
        } else if self.0.is_status() {
            write!(f, "Redacted Reqwest Error: Status")
        } else if self.0.is_body() {
            write!(f, "Redacted Reqwest Error: Body")
        } else if self.0.is_decode() {
            write!(f, "Redacted Reqwest Error: Decode")
        } else {
            write!(f, "Redacted Reqwest Error")
        }
    }
}

impl Debug for RedactableReqwestError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

#[derive(Error, Clone, Serialize, Deserialize)]
pub struct RedactableRelayErrorResponse {
    pub code: Option<u64>,
    pub message: String,
}

impl std::fmt::Display for RedactableRelayErrorResponse {
    #[cfg(not(feature = "redact-sensitive"))]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Relay error: (code: {}, message: {})",
            self.code.unwrap_or_default(),
            self.message
        )
    }

    #[cfg(feature = "redact-sensitive")]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Relay error: (code: {}, message: [REDACTED])",
            self.code.unwrap_or_default(),
        )
    }
}

impl Debug for RedactableRelayErrorResponse {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}
