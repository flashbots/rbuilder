//! Sadly we need to improve out builder shutdown procedure :(
//! We have some places where we abruptly kill the process (eg: watchdog, bidding service communication errors) but
//! some modules need to finish their work so we must give them some time before killing the process.
//! Here we centralize all this hacky stuff so at least we can see all the constants in one place.

use std::{io::Write, time::Duration};

use alloy_eips::merge::SLOT_DURATION_SECS;
use tokio_util::sync::CancellationToken;
use tracing::error;

/// Time for the run_submit_to_relays_job to stop submitting blocks after the cancellation token is cancelled.
/// It's just a loop that signs blocks and submits them async (on detached tasks) so if should not take more than a second.
pub const RUN_SUBMIT_TO_RELAYS_JOB_CANCEL_TIME_SECONDS: u64 = 1;

/// Time for the block building to close after the cancellation token is cancelled.
/// We use a whole block as heuristic for the time to close.
pub const BLOCK_BUILDING_CLOSE_TIME_SECONDS: u64 = SLOT_DURATION_SECS;

pub const RUN_SUBMIT_TO_RELAYS_JOB_CANCEL_TIME: Duration =
    Duration::from_secs(RUN_SUBMIT_TO_RELAYS_JOB_CANCEL_TIME_SECONDS);

/// This time should be enough to let the process to finish its work and exit gracefully.
/// Example of this need is the clickhouse backup that takes a while to finish and we don't want to loose any blocks.
/// This should be > than everything we have to wait for in the constants above.
pub const MAX_WAIT_TIME_SECONDS: u64 = BLOCK_BUILDING_CLOSE_TIME_SECONDS;
pub const MAX_WAIT_TIME: Duration = Duration::from_secs(MAX_WAIT_TIME_SECONDS);

/// Time needed to let the tracing subscriber to flush its buffers.
pub const FLUSH_TRACE_TIME: Duration = Duration::from_millis(200);

/// Time we wait before killing the process abruptly in ProcessKiller::kill().
/// We add 1 second to allow the process to finish its work and exit gracefully.
pub const PROCESS_KILLER_WAIT_TIME: Duration = Duration::from_secs(MAX_WAIT_TIME_SECONDS + 1);

#[derive(Debug, Clone)]
pub struct ProcessKiller {
    cancellation_token: CancellationToken,
}

impl ProcessKiller {
    pub fn new(cancellation_token: CancellationToken) -> Self {
        Self { cancellation_token }
    }

    pub fn kill(&self, reason: &str) {
        error!(
            reason,
            wait_time_secs = PROCESS_KILLER_WAIT_TIME.as_secs(),
            "Process killing started, signaling cancellation token and waiting"
        );
        self.cancellation_token.cancel();
        Self::wait_and_kill(reason);
    }

    /// Waits some time to give the process a chance to finish its work and exit gracefully and then kills it abruptly.
    pub fn wait_and_kill(reason: &str) {
        std::thread::sleep(PROCESS_KILLER_WAIT_TIME);
        error!(reason, "Killing process");
        ensure_tracing_buffers_flushed();
        std::process::exit(1);
    }
}

/// Tries to guarantee that all tracing is flushed so we don't loose any final messages.
pub fn ensure_tracing_buffers_flushed() {
    // Flush the stdout and stderr buffers so all tracing messages are flushed.
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // Small delay to let any async work complete so flushed buffers are actually flushed.
    std::thread::sleep(FLUSH_TRACE_TIME);
}
