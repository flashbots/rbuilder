//! In some installations (eg: TDX) we may want to avoid ever exposing some information.
//! This module contains all redacting stuff to achieve that.
//! We try to keep this redacting always configurable so we can have the full data for development.
pub mod error_redactor;
pub mod relay;
pub mod validation;

/// Constant for redacted content
const REDACTED: &str = "[REDACTED]";
