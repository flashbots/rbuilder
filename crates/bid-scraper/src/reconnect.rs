use exponential_backoff::Backoff;
use std::{future::Future, time::Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, info_span, warn, Instrument};

#[derive(Debug)]
pub enum RunCommand {
    Reconnect,
    Finish,
}

fn default_backoff() -> Backoff {
    Backoff::new(u32::MAX, Duration::from_secs(1), Duration::from_secs(12))
}

pub async fn run_async_loop_with_reconnect<
    Connection,
    ConnectErr: std::error::Error,
    ConnectFut: Future<Output = Result<Connection, ConnectErr>>,
    RunFut: Future<Output = RunCommand>,
    Connect: Fn() -> ConnectFut,
    Run: Fn(Connection) -> RunFut,
>(
    context: &str,
    connect: Connect,
    run: Run,
    backoff: Option<Backoff>,
    cancellation_token: CancellationToken,
) {
    let span = info_span!("connect_loop_context", context);

    'reconnect: loop {
        if cancellation_token.is_cancelled() {
            break 'reconnect;
        }
        let backoff = backoff.clone().unwrap_or_else(default_backoff);
        let mut backoff_iter = backoff.iter();

        let connection = 'backoff: loop {
            let timeout = if let Some(timeout) = backoff_iter.next() {
                timeout
            } else {
                warn!(parent: &span, "Backoff for connection reached max retries");
                break 'reconnect;
            };

            match connect().instrument(span.clone()).await {
                Ok(conn) => {
                    debug!(parent: &span, "Established connection");
                    break 'backoff conn;
                }
                Err(err) => {
                    error!(parent: &span, ?err, "Failed to establish connection");
                    tokio::time::sleep(timeout).await;
                }
            }
        };

        match run(connection).instrument(span.clone()).await {
            RunCommand::Reconnect => continue 'reconnect,
            RunCommand::Finish => break 'reconnect,
        }
    }
    info!("Exiting connect loop");
}
