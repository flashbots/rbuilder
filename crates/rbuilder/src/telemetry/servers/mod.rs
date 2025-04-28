//! Telemetry contains two servers.
//!
//! - [full]: verbose server exposing detailed operational information about the
//!   builder.
//! - [redacted]: deliberately redacted server serves information suitable for
//!   tdx builders to expose in real-time.
//!
//! The redacted server is seperate from the debug server because it may be desirable
//! to expose debug and redacted data differently in tdx builders. e.g. redacted data
//! immediately avaliable, debug data avaliable after a delay or some seperate sanitisation.

pub mod full;
pub mod redacted;
