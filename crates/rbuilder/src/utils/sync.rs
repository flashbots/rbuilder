use std::time::Duration;

use parking_lot::{Condvar, Mutex};

/// Similar to tokio::sync::Watch but for threads.
/// It allows to communicate a producer and a consumer in the escenario where the consumer only cares about the last value.
#[derive(Debug, Default)]
pub struct Watch<T> {
    last_data: Mutex<Option<T>>,
    got_data: Condvar,
}

/// Hardcoded timeout for waiting for data.
/// Reasonably high so a loop does not has impact in the CPU and low enough so teardown is not too long.
pub const THREAD_BLOCKING_DURATION: Duration = Duration::from_millis(100);

impl<T> Watch<T> {
    pub fn new() -> Self {
        Self {
            last_data: Mutex::new(None),
            got_data: Condvar::new(),
        }
    }

    /// Sets the data and wakes up the consumer.
    pub fn set(&self, data: T) {
        let mut guard = self.last_data.lock();
        *guard = Some(data);
        self.got_data.notify_one();
    }

    /// Waits for data new data a hardcoded timeout.
    pub fn wait_for_data(&self) -> Option<T> {
        let mut guard = self.last_data.lock();
        while guard.is_none() {
            let timeout_result = self.got_data.wait_for(&mut guard, THREAD_BLOCKING_DURATION);
            if timeout_result.timed_out() {
                return None;
            }
        }
        guard.take()
    }

    /// USE WITH CAUTION.
    /// It will wait forever for data.
    pub fn wait_for_ever(&self) -> T {
        loop {
            if let Some(res) = self.wait_for_data() {
                return res;
            }
        }
    }
}
