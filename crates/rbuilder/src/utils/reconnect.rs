use exponential_backoff::Backoff;
use std::{thread::sleep, time::Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, info_span, warn};

#[derive(Debug)]
pub enum RunCommand {
    Reconnect,
    Finish,
}

fn backoff() -> Backoff {
    Backoff::new(u32::MAX, Duration::from_secs(1), Duration::from_secs(12))
}

/// Helper function for the typical scenario where we connect to something, process data, connection fails and we continuously try to reconnect.
/// connect is continuously called until it returns Ok and then run is called.
/// run's result decides if we end or go back to reconnecting.
pub fn run_loop_with_reconnect<
    Connection,
    ConnectErr: std::error::Error,
    Connect: Fn() -> Result<Connection, ConnectErr>,
    Run: Fn(Connection) -> RunCommand,
>(
    context: &str,
    connect: Connect,
    run: Run,
    cancellation_token: CancellationToken,
) {
    let span = info_span!("connect_loop_context", context);
    let _span_guard = span.enter();

    'reconnect: loop {
        if cancellation_token.is_cancelled() {
            break 'reconnect;
        }

        let backoff = backoff();
        let mut backoff_iter = backoff.iter();
        let connection = 'backoff: loop {
            let timeout = if let Some(timeout) = backoff_iter.next() {
                timeout
            } else {
                warn!("Backoff for connection reached max retries");
                break 'reconnect;
            };
            match connect() {
                Ok(connection) => {
                    debug!("Established connection");
                    break 'backoff connection;
                }
                Err(err) => {
                    error!(?err, "Failed to establish connection");
                    sleep(timeout);
                }
            };
        };

        match run(connection) {
            RunCommand::Reconnect => continue 'reconnect,
            RunCommand::Finish => break 'reconnect,
        }
    }

    info!("Exiting connect loop");
}
