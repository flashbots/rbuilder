//! Helpful macros spawning clickhouse indexer tasks.

// Rationale: a simple text-replacement macro was much more effective compared to fighting the
// compiler with additional trait bounds on the [`clickhouse::Row`] trait.

#[macro_export]
macro_rules! spawn_clickhouse_inserter {
    ($executor:ident, $runner:ident, $name:expr, $target:expr) => {{
        $executor.spawn_with_graceful_shutdown_signal(|shutdown| async move {
            let mut shutdown_guard = None;
            tokio::select! {
                _ = $runner.run_loop() => {
                    tracing::info!(target: $target, "clickhouse {} indexer channel closed", $name);
                }
                guard = shutdown => {
                    tracing::info!(target: $target, "Received shutdown for {} indexer, performing cleanup", $name);
                    shutdown_guard = Some(guard);
                },
            }

            match $runner.end().await {
                Ok(quantities) => {
                    tracing::info!(target: $target, ?quantities, "finalized clickhouse {} inserter", $name);
                }
                Err(e) => {
                    tracing::error!(target: $target, ?e, "failed to write end insertion of {} to indexer", $name);
                }
            }

            drop(shutdown_guard);
        });
    }};
}

#[macro_export]
macro_rules! spawn_clickhouse_backup {
    ($executor:ident, $backup:ident, $name: expr, $target:expr) => {{
        $executor.spawn_with_graceful_shutdown_signal(|shutdown| async move {
            let mut shutdown_guard = None;
            tokio::select! {
                _ = $backup.run() => {
                    tracing::info!(target: $target, "clickhouse {} backup channel closed", $name);
                }
                guard = shutdown => {
                    tracing::info!(target: $target, "Received shutdown for {} backup, performing cleanup", $name);
                    shutdown_guard = Some(guard);
                },
            }

            $backup.end().await;
            drop(shutdown_guard);
        });
    }};
}
