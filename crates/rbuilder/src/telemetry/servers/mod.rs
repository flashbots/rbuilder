//! Telemetry contains two servers.
//!
//! - debug: verbose server exposing detailed operational information about the
//!          builder.
//! - opaque: deliberately opaque server serves information suitable for
//!           tdx builders to expose in real-time.
//!
//! The opaque server is seperate from the debug server because it may be desirable
//! to expose debug and opaque data differently in tdx builders. e.g. opaque data
//! immediately avaliable, debug data avaliable after a delay or some seperate sanitisation.

pub mod debug;
pub mod opaque;
