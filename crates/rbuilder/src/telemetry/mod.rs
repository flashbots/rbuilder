//! Telemetry modules helps tracking what is happening in the rbuilder.
//!
//! The restricted server is seperate from the full server, because it may be desirable
//! to expose full and restricted data differently in tdx builders. e.g. restricted data
//! immediately avaliable, and full data avaliable after a delay or some seperate sanitisation.

mod dynamic_logs;
mod metrics;
pub mod servers;

pub use dynamic_logs::*;
pub use metrics::*;
