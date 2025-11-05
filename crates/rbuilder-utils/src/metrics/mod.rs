use std::time::{Duration, Instant};

/// A simple sampler that executes a closure every `sample_size` calls, or if a certain amount of
/// time has passed since last sampling call.
#[derive(Debug, Clone)]
pub struct Sampler {
    sample_size: usize,
    counter: usize,
    start: Instant,
    interval: Duration,
}

impl Default for Sampler {
    fn default() -> Self {
        Self {
            sample_size: 4096,
            counter: 0,
            start: Instant::now(),
            interval: Duration::from_secs(10),
        }
    }
}

impl Sampler {
    pub fn with_sample_size(mut self, sample_size: usize) -> Self {
        self.sample_size = sample_size;
        self
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.start = Instant::now() - interval;
        self
    }

    /// Call this function to potentially execute the sample closure if we have reached the sample
    /// size, or enough time has passed. Otherwise, it increments the internal counter.
    pub fn sample(&mut self, f: impl FnOnce()) {
        if self.counter >= self.sample_size || self.start.elapsed() >= self.interval {
            self.counter = 0;
            self.start = Instant::now();
            f();
        } else {
            self.counter += 1;
        }
    }
}
