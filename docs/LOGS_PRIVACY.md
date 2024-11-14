# Logs Privacy in rbuilder

## Introduction

Log privacy in rbuilder refers to the level of data exposed in logs via macros like `error!`. A key principle in rbuilder is that we never log a full order, as this information could be harmful to the order sender.

### Why is this important?

-   A non-landed order, if logged in full, could potentially be executed in a later block, causing losses for the order owner.
-   Even if an order has built-in protections against unexpected executions, the order owner might still incur gas fees.

## External Error Redaction

While we don't log full orders ourselves, we sometimes interact with external systems and log their error codes. Since some of these may contain plain strings, we offer an option to redact any error before logging.

### Enabling Error Redaction

To enable external error redaction, use the `redact-sensitive` feature flag.

### Example of Error Redaction

**Never** derive `Display` or `Debug` for errors which may contain sensitive info.

Instead, explicitly implement them using your favourite library, or manually.

```rust
#[derive(Error)]
pub enum SomeError {
    #[error("Request error: {0}")]
    RequestError(#[from] RedactableReqwestError),

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
        write!(f, "{}", self)
    }
}

#[derive(Error)]
pub struct RedactableReqwestError(reqwest::Error);

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
```
