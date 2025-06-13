//! We include here all the info to reproduce everything that happened during the slot.

use crate::live_builder::order_input::ReplaceableOrderPoolCommand;

/// A ReplaceableOrderPoolCommand + timestamp to be able to reproduce the orderflow timeline.
#[derive(Debug, Clone)]
pub struct ReplaceableOrderPoolCommandWithTimestamp {
    pub timestamp_ms: u64,
    pub command: ReplaceableOrderPoolCommand,
}
