use std::time::{Duration, Instant};

/// Simple struct to measure the time since this is created until it dies to log delta on metrics
/// Fn is used instead of FnOnce to avoid life problems on drop.
pub struct ScopeMeter<Callback: Fn(Duration)> {
    start: Instant,
    callback: Callback,
}

impl<Callback: Fn(Duration)> ScopeMeter<Callback> {
    pub fn new(callback: Callback) -> Self {
        Self {
            start: Instant::now(),
            callback,
        }
    }
}

impl<Callback: Fn(Duration)> Drop for ScopeMeter<Callback> {
    fn drop(&mut self) {
        (self.callback)(self.start.elapsed())
    }
}
