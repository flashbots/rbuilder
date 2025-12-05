use crate::live_builder::process_killer::ProcessKiller;
use flume::RecvTimeoutError;
use std::{io, time::Duration};

/// Spawns a thread that will kill the process if there is no events sent on the channel
/// for the timeout time.
/// context is a string to be logged to be able to distinguish different types of deaths.
pub fn spawn_watchdog_thread(
    timeout: Duration,
    context: String,
    process_killer: ProcessKiller,
) -> io::Result<flume::Sender<()>> {
    let (sender, receiver) = flume::unbounded();
    std::thread::Builder::new()
        .name(String::from("watchdog"))
        .spawn(move || loop {
            match receiver.recv_timeout(timeout) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => {
                    process_killer.kill(format!("Watchdog timeout: {}", context).as_str());
                }
                Err(RecvTimeoutError::Disconnected) => {
                    break;
                }
            }
        })?;
    Ok(sender)
}
