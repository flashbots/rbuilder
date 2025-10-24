//! Time-related utilies.

use std::{
    future::{poll_fn, Future as _},
    iter::Iterator,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

/// A random number generator for applying jitter to [`std::time::Duration`].
#[derive(Debug, Clone)]
pub(crate) struct Jitter;

impl Jitter {
    /// Apply jitter to provided duration, by multiplying it for a random number between 0 and 2.
    pub(crate) fn apply_to(duration: Duration) -> Duration {
        duration.mul_f64(rand::random::<f64>() * 2_f64)
    }
}

/// A retry strategy driven by exponential back-off.
///
/// The power corresponds to the number of past attempts.
///
/// Taken from <https://docs.rs/tokio-retry/latest/src/tokio_retry/strategy/exponential_backoff.rs.html>
#[derive(Debug, Clone)]
pub(crate) struct ExponentialBackoff {
    current: u64,
    base: u64,
    factor: u64,
    max_delay: Option<Duration>,
}

#[allow(dead_code)]
impl ExponentialBackoff {
    /// Constructs a new exponential back-off strategy,
    /// given a base duration in milliseconds.
    ///
    /// The resulting duration is calculated by taking the base to the `n`-th power,
    /// where `n` denotes the number of past attempts.
    pub(crate) fn from_millis(base: u64) -> ExponentialBackoff {
        ExponentialBackoff {
            current: base,
            base,
            factor: 1u64,
            max_delay: None,
        }
    }

    /// A multiplicative factor that will be applied to the retry delay.
    ///
    /// For example, using a factor of `1000` will make each delay in units of seconds.
    ///
    /// Default factor is `1`.
    pub(crate) fn factor(mut self, factor: u64) -> ExponentialBackoff {
        self.factor = factor;
        self
    }

    /// Apply a maximum delay. No retry delay will be longer than this `Duration`.
    pub(crate) fn max_delay(mut self, duration: Duration) -> ExponentialBackoff {
        self.max_delay = Some(duration);
        self
    }

    /// Reset the backoff to the initial state.
    pub(crate) fn reset(&mut self) {
        self.current = self.base;
    }
}

impl Iterator for ExponentialBackoff {
    type Item = Duration;

    // TODO: change this logic, so that we can always multiply base by a factor.
    // e.g. base = 8, factor = 2 yields to: 8ms, 16ms, 32ms, 64ms, ...
    fn next(&mut self) -> Option<Duration> {
        // set delay duration by applying factor
        let duration = if let Some(duration) = self.current.checked_mul(self.factor) {
            Duration::from_millis(duration)
        } else {
            Duration::from_millis(u64::MAX)
        };

        // check if we reached max delay
        if let Some(ref max_delay) = self.max_delay {
            if duration > *max_delay {
                return Some(*max_delay);
            }
        }

        if let Some(next) = self.current.checked_mul(self.base) {
            self.current = next;
        } else {
            self.current = u64::MAX;
        }

        Some(duration)
    }
}

/// An interval heavily inspired by [`tokio::time::Interval`], that supports exponential back-off
/// and jitter.
#[derive(Debug)]
pub(crate) struct BackoffInterval {
    /// Future that completes the next time the `Interval` yields a value.
    delay: Pin<Box<tokio::time::Sleep>>,

    /// The exponential backoff configuration.
    backoff: ExponentialBackoff,

    /// An optional jitter to apply to the ticks.
    jitter: bool,
}

impl BackoffInterval {
    /// Creates a new interval that ticks immediately.
    pub(crate) fn new(backoff: ExponentialBackoff) -> Self {
        let start = tokio::time::Instant::now();
        let delay = Box::pin(tokio::time::sleep_until(start));
        Self {
            delay,
            backoff,
            jitter: false,
        }
    }

    pub(crate) fn with_jitter(mut self) -> Self {
        self.jitter = true;
        self
    }

    pub(crate) fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<tokio::time::Instant> {
        // Wait for the delay to be done
        std::task::ready!(Pin::new(&mut self.delay).poll(cx));

        // Get the time when we were schedulued to tick
        let timeout = self.delay.deadline();

        // CHANGE: use custom logic that takes into a account backoff and jitter to calculate new
        // instant.
        let next = self.next();

        // CHANGE: Unfortunately, [`tokio::time::Sleep::reset_without_reregister`] isn't
        // pub(crate)lic so we have to register the waker again.
        self.delay.as_mut().reset(next);

        Poll::Ready(timeout)
    }

    /// Completes when the next instant in the interval has been reached.
    pub(crate) async fn tick(&mut self) -> tokio::time::Instant {
        let instant = poll_fn(|cx| self.poll_tick(cx));

        instant.await
    }

    /// Resets backoff to the initial state, and the next tick will happen after the initial period
    /// returned by [`ExponentialBackoff`].
    pub(crate) fn reset(&mut self) {
        self.backoff.reset();
        let next = self.next();
        self.delay.as_mut().reset(next);
    }

    /// Return the next instant at which the interval should tick.
    fn next(&mut self) -> tokio::time::Instant {
        let now = tokio::time::Instant::now();
        // We provide a [`tokio::time::MissedTickBehavior::Delay`] behavior but we also add backoff
        // and jitter if the user configured it.
        let mut period = self
            .backoff
            .next()
            .expect("ExponentialBackoff never returns None");
        if self.jitter {
            period = Jitter::apply_to(period);
        }
        now.checked_add(period).expect("no overflow")
    }
}

impl Default for BackoffInterval {
    fn default() -> Self {
        // So will return 4, 16, 64, 256, 1024, ... milliseconds with jitter.
        Self::new(ExponentialBackoff::from_millis(4).max_delay(Duration::from_millis(8192)))
            .with_jitter()
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::{Duration, Instant};

    use super::*;

    #[test]
    fn exp_backoff_returns_some_exponential_base_10() {
        let mut s = ExponentialBackoff::from_millis(10);

        assert_eq!(s.next(), Some(Duration::from_millis(10)));
        assert_eq!(s.next(), Some(Duration::from_millis(100)));
        assert_eq!(s.next(), Some(Duration::from_millis(1000)));
    }

    #[test]
    fn exp_backoff_returns_some_exponential_base_2() {
        let mut s = ExponentialBackoff::from_millis(2);

        assert_eq!(s.next(), Some(Duration::from_millis(2)));
        assert_eq!(s.next(), Some(Duration::from_millis(4)));
        assert_eq!(s.next(), Some(Duration::from_millis(8)));
    }

    #[test]
    fn exp_backoff_saturates_at_maximum_value() {
        let mut s = ExponentialBackoff::from_millis(u64::MAX - 1);

        assert_eq!(s.next(), Some(Duration::from_millis(u64::MAX - 1)));
        assert_eq!(s.next(), Some(Duration::from_millis(u64::MAX)));
        assert_eq!(s.next(), Some(Duration::from_millis(u64::MAX)));
    }

    #[test]
    fn exp_backoff_can_use_factor_to_get_seconds() {
        let factor = 1000;
        let mut s = ExponentialBackoff::from_millis(2).factor(factor);

        assert_eq!(s.next(), Some(Duration::from_secs(2)));
        assert_eq!(s.next(), Some(Duration::from_secs(4)));
        assert_eq!(s.next(), Some(Duration::from_secs(8)));
    }

    #[test]
    fn exp_backoff_stops_increasing_at_max_delay() {
        let mut s = ExponentialBackoff::from_millis(2).max_delay(Duration::from_millis(4));

        assert_eq!(s.next(), Some(Duration::from_millis(2)));
        assert_eq!(s.next(), Some(Duration::from_millis(4)));
        assert_eq!(s.next(), Some(Duration::from_millis(4)));
    }

    #[test]
    fn exp_backoff_returns_max_when_max_less_than_base() {
        let mut s = ExponentialBackoff::from_millis(20).max_delay(Duration::from_millis(10));

        assert_eq!(s.next(), Some(Duration::from_millis(10)));
        assert_eq!(s.next(), Some(Duration::from_millis(10)));
    }

    // Tests with `start_paused = true` consists of tests with [`tokio::time::pause`] and
    // require manual advancement of time with [`tokio::time::advance`] or with sleeps.

    #[tokio::test(start_paused = true)]
    async fn backoff_interval_ticks_as_expected() {
        let backoff = ExponentialBackoff::from_millis(2);
        let mut backoff_clone = backoff.clone();
        let mut interval = BackoffInterval::new(backoff);

        let before = Instant::now();
        let t1 = interval.tick().await;
        assert_eq!(t1, before);
        let t2 = interval.tick().await;
        assert_eq!(t2, t1 + backoff_clone.next().unwrap());
        let t3 = interval.tick().await;
        assert_eq!(t3, t2 + backoff_clone.next().unwrap());
        let t4 = interval.tick().await;
        assert_eq!(t4, t3 + backoff_clone.next().unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_interval_resets_properly() {
        let backoff = ExponentialBackoff::from_millis(2);
        let mut backoff_clone = backoff.clone();
        let mut interval = BackoffInterval::new(backoff);

        interval.tick().await;
        interval.tick().await;
        interval.tick().await;
        interval.tick().await;

        interval.reset();
        let now = Instant::now();
        let expected_delay = backoff_clone.next().unwrap();
        let actual = interval.tick().await;

        assert_eq!(now + expected_delay, actual);
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_interval_with_jitter_works() {
        // No jitter
        {
            let beginning = Instant::now();

            let backoff = ExponentialBackoff::from_millis(5);
            let mut backoff_clone = backoff.clone();
            let mut interval = BackoffInterval::new(backoff);

            let t1 = interval.tick().await;
            assert_eq!(t1, beginning); // First tick is immediate

            let t2 = interval.tick().await;
            assert_eq!(t2, t1 + backoff_clone.next().unwrap());

            let t3 = interval.tick().await;
            assert_eq!(t3, t2 + backoff_clone.next().unwrap());
        }

        // Jitter
        {
            let beginning = Instant::now();

            let backoff = ExponentialBackoff::from_millis(5);
            let mut backoff_clone = backoff.clone();
            let mut interval = BackoffInterval::new(backoff).with_jitter();
            let t1 = interval.tick().await;
            assert_eq!(t1, beginning); // First tick is immediate

            // Next tick will be 5ms later, but jitter changes it.
            let t2 = interval.tick().await;
            assert_ne!(t2, t1 + backoff_clone.next().unwrap());
        }
    }
}
