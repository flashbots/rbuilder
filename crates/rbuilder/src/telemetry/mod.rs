//! Telemetry modules helps tracking what is happening in the rbuilder.
//!
//! The opaque server is seperate from the debug server, because it may be desirable
//! to expose debug and opaque data differently in tdx builders. e.g. opaque data
//! immediately avaliable, debug data avaliable after a delay or some seperate sanitisation.

mod dynamic_logs;
mod metrics;
pub mod servers;

pub use dynamic_logs::*;
pub use metrics::*;
