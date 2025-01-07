use flume::RecvTimeoutError;
use std::{io, time::Duration};
use tracing::{error, info};

/// Spawns a thread that will kill the process if there is no events sent on the channel
/// for the timeout time.
/// context is a string to be logged to be able to distinguish different types of deaths.
pub fn spawn_watchdog_thread(timeout: Duration, context: String) -> io::Result<flume::Sender<()>> {
    let (sender, receiver) = flume::unbounded();
    std::thread::Builder::new()
        .name(String::from("watchdog"))
        .spawn(move || {
            loop {
                match receiver.recv_timeout(timeout) {
                    Ok(()) => {}
                    Err(RecvTimeoutError::Timeout) => {
                        error!(context, "Watchdog timeout");
                        std::process::exit(1);
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        break;
                    }
                }
            }
            info!(
                context,
                "Watchdog finished, will kill application in 12 seconds"
            );

            std::thread::sleep(Duration::from_secs(12));
            std::process::exit(1);
        })?;

    Ok(sender)
}
