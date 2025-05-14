//! Error storage is used to store example of errors for the future reproduction
//! It works as a separate thread that write payload with attached to the sqlite db
//! Writer limits amount of errors that can be written to the db.

use crossbeam_queue::ArrayQueue;
use lazy_static::lazy_static;
use parking_lot::Mutex;
use sqlx::{sqlite::SqliteConnectOptions, ConnectOptions, Executor, SqliteConnection};
use std::{path::Path, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tracing::{error, info_span, warn};

// To prevent db from being overwhelmed with errors we store only a limited amount of errors per category
// after this limit is reached we stop writing errors to the db
const MAX_ERRORS_PER_CATEGORY: usize = 100;

// Maximum size of the payload in bytes (100 mb)
const MAX_PAYLOAD_SIZE_BYTES: usize = 100_000_000;

// If events don't get processed fst enough we drop them.
const MAX_PENDING_EVENTS: usize = 500;

#[derive(Debug)]
struct ErrorEvent {
    category: String,
    error: String,
    payload: String,
}

lazy_static! {
    /// Not using null object pattern due to some generic on trait problems.
    static ref EVENT_QUEUE: Mutex<Option<Arc<ArrayQueue<ErrorEvent>>>> = Mutex::new(None);
}

fn event_queue() -> Option<Arc<ArrayQueue<ErrorEvent>>> {
    EVENT_QUEUE.lock().clone()
}

/// Spawn a new error storage writer.
/// `db_path` is a path to the sqlite db file
pub async fn spawn_error_storage_writer(
    db_path: impl AsRef<Path>,
    global_cancel: CancellationToken,
) -> eyre::Result<()> {
    let mut storage = ErrorEventStorage::new_from_path(db_path).await?;
    *EVENT_QUEUE.lock() = Some(Arc::new(ArrayQueue::new(MAX_PENDING_EVENTS)));
    tokio::spawn(async move {
        while !global_cancel.is_cancelled() {
            if let Some(event_queue) = event_queue() {
                if let Some(event) = event_queue.pop() {
                    if let Err(err) = storage.write_error_event(&event).await {
                        warn!(
                            category = event.category,
                            ?err,
                            "Error writing error event to storage"
                        );
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            } else {
                error!("error_storage writing task has no event queue");
                return;
            }
        }
    });
    Ok(())
}

/// Store an error event to be written to the error storage
/// This function is non-blocking. If the error storage is full, the error will be dropped.
/// `category` is a string that describes the category of the error
/// `error` is a string that describes the error
/// `payload` is a serializable payload that will be stored with the error as a json
pub fn store_error_event<T: serde::Serialize>(category: &str, error: &str, payload: T) {
    if let Some(event_queue) = event_queue() {
        let span = info_span!("store_error_event", category, error);
        let _span_guard = span.enter();

        let payload_json = match serde_json::to_string(&payload) {
            Ok(res) => res,
            Err(err) => {
                error!(?err, "Error serializing error payload");
                return;
            }
        };
        if payload_json.len() > MAX_PAYLOAD_SIZE_BYTES {
            error!(
                payload_size = payload_json.len(),
                "Error payload is too large, not storing error event"
            );
            return;
        }
        if event_queue
            .push(ErrorEvent {
                category: category.to_string(),
                error: error.to_string(),
                payload: payload_json,
            })
            .is_err()
        {
            error!("Error storing error event, queue is full.");
        }
    }
}

#[derive(Debug)]
struct ErrorEventStorage {
    conn: SqliteConnection,
}

impl ErrorEventStorage {
    async fn new_from_path(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let mut res = Self {
            conn: SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .connect()
                .await?,
        };
        res.create_tables().await?;
        Ok(res)
    }

    #[allow(dead_code)]
    async fn new_from_memory() -> eyre::Result<Self> {
        let mut res = Self {
            conn: SqliteConnectOptions::new().connect().await?,
        };
        res.create_tables().await?;
        Ok(res)
    }

    async fn create_tables(&mut self) -> eyre::Result<()> {
        self.conn
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS error_events (
                category TEXT NOT NULL,
                error TEXT NOT NULL,
                payload TEXT NOT NULL
            );
            "#,
            )
            .await?;

        Ok(())
    }

    async fn write_error_event(&mut self, event: &ErrorEvent) -> eyre::Result<()> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM error_events WHERE category = ?")
            .bind(&event.category)
            .fetch_one(&mut self.conn)
            .await?;

        if count as usize >= MAX_ERRORS_PER_CATEGORY {
            warn!(
                category = &event.category,
                "Error storage is full, not writing error event"
            );
            return Ok(());
        }

        sqlx::query(
            r#"
            INSERT INTO error_events (category, error, payload)
            VALUES (?, ?, ?)
            "#,
        )
        .bind(&event.category)
        .bind(&event.error)
        .bind(&event.payload)
        .execute(&mut self.conn)
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_error_storage() {
        let mut storage = ErrorEventStorage::new_from_memory().await.unwrap();

        let event = ErrorEvent {
            category: "test".to_string(),
            error: "test error".to_string(),
            payload: "test payload".to_string(),
        };

        for _ in 0..MAX_ERRORS_PER_CATEGORY + 5 {
            storage.write_error_event(&event).await.unwrap();
        }
    }
}
