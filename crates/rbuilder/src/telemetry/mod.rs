//! Telemetry modules helps tracking what is happening in the rbuilder.
//!
//! The redacted server is seperate from the full server, because it may be desirable
//! to expose full and redacted data differently in tdx builders. e.g. redacted data
//! immediately avaliable, and full data avaliable after a delay or some seperate sanitisation.

mod metrics;
pub mod servers;

pub use metrics::*;
