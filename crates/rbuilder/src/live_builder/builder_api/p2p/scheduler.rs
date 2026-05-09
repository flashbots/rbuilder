use super::types::EpbsP2PConfig;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Manages bid timing within slots.
/// Manages time-based bid submission within slot boundaries.
/// Supports two modes:
/// interval mode: resubmit bids at regular intervals within the bidding window
/// single bid mode: submit once when payload is ready
#[derive(Debug, Clone)]
pub struct BidScheduler {
    /// Genesis time in seconds since Unix epoch.
    genesis_time: u64,
    /// Slot duration in seconds.
    seconds_per_slot: u64,
    /// Ms relative to slot start when bidding opens (negative = before slot start).
    bid_start_ms: i64,
    /// Ms relative to slot start when bidding closes (typically positive).
    bid_end_ms: i64,
    /// Interval between bid resubmissions, 0 = single bid mode.
    bid_interval_ms: u64,
}

impl BidScheduler {
    pub fn new(config: &EpbsP2PConfig) -> Self {
        Self {
            genesis_time: config.genesis_time,
            seconds_per_slot: config.seconds_per_slot,
            bid_start_ms: config.bid_start_ms,
            bid_end_ms: config.bid_end_ms,
            bid_interval_ms: config.bid_interval_ms,
        }
    }

    /// Returns the slot start time as seconds since unix epoch.
    pub fn slot_start_time(&self, slot: u64) -> u64 {
        self.genesis_time + slot * self.seconds_per_slot
    }

    /// Returns ms relative to the start of the given slot.
    /// Negative when wall-clock is before the slot starts; positive once the
    /// slot has begun. Computed in i128 to avoid overflow on the subtraction
    /// before truncating to i64 at the end.
    pub fn ms_relative_to_slot(&self, slot: u64) -> i64 {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i128;
        let slot_start_ms = (self.slot_start_time(slot) as i128) * 1000;
        (now_ms - slot_start_ms) as i64
    }

    /// Returns true if we are currently within the bidding window for the given slot.
    /// Window: [bid_start_ms, bid_end_ms) relative to slot start.
    pub fn is_in_bidding_window(&self, slot: u64) -> bool {
        let rel = self.ms_relative_to_slot(slot);
        rel >= self.bid_start_ms && rel < self.bid_end_ms
    }

    /// Returns the duration until the bidding window opens for the given slot.
    /// Returns Duration::ZERO if the window is already open.
    /// Returns None if the bidding window has already closed.
    pub fn time_until_bid_start(&self, slot: u64) -> Option<Duration> {
        let rel = self.ms_relative_to_slot(slot);
        if rel >= self.bid_end_ms {
            return None;
        }
        if rel >= self.bid_start_ms {
            return Some(Duration::ZERO);
        }
        Some(Duration::from_millis((self.bid_start_ms - rel) as u64))
    }

    /// Returns the duration until the bidding window closes for the given slot.
    /// Returns None if the window has already closed.
    pub fn time_until_bid_end(&self, slot: u64) -> Option<Duration> {
        let rel = self.ms_relative_to_slot(slot);
        if rel >= self.bid_end_ms {
            return None;
        }
        let remaining = self.bid_end_ms - rel;
        Some(Duration::from_millis(remaining.max(0) as u64))
    }

    /// Whether this scheduler is in single bid mode i.e no resubmission in the slot.
    pub fn is_single_bid_mode(&self) -> bool {
        self.bid_interval_ms == 0
    }

    /// Returns the bid resubmission interval or none in single bid mode.
    pub fn bid_interval(&self) -> Option<Duration> {
        if self.bid_interval_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(self.bid_interval_ms))
        }
    }

    /// Compute the current slot from wall clock time.
    pub fn current_slot(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if now < self.genesis_time {
            return 0;
        }
        (now - self.genesis_time) / self.seconds_per_slot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_scheduler(genesis_time: u64) -> BidScheduler {
        BidScheduler {
            genesis_time,
            seconds_per_slot: 12,
            bid_start_ms: -1000,
            bid_end_ms: 1000,
            bid_interval_ms: 250,
        }
    }

    #[test]
    fn test_slot_start_time() {
        let s = make_scheduler(1_000_000);
        assert_eq!(s.slot_start_time(0), 1_000_000);
        assert_eq!(s.slot_start_time(1), 1_000_012);
        assert_eq!(s.slot_start_time(100), 1_001_200);
    }

    #[test]
    fn test_single_bid_mode() {
        let mut s = make_scheduler(0);
        assert!(!s.is_single_bid_mode());
        s.bid_interval_ms = 0;
        assert!(s.is_single_bid_mode());
    }

    #[test]
    fn test_bid_interval() {
        let s = make_scheduler(0);
        assert_eq!(s.bid_interval(), Some(Duration::from_millis(250)));
    }

    #[test]
    fn test_ms_relative_to_slot_negative_pre_slot() {
        // build a scheduler whose slot 1 starts ~1 hour in the future. The
        // relative time for slot 1 should be a large negative number.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let s = make_scheduler(now_secs + 3600);
        let rel = s.ms_relative_to_slot(0);
        assert!(rel < 0, "expected negative rel, got {}", rel);
        assert!(!s.is_in_bidding_window(0));
    }

    #[test]
    fn test_ms_relative_to_slot_in_window() {
        // set genesis to current second so slot 0 starts at the rounded down
        // second of now. ms_relative_to_slot(0) then equals now_ms % 1000,
        // which is in [0, 999] — always inside the [-1000, +1000) window.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let s = make_scheduler(now_secs);
        let rel = s.ms_relative_to_slot(0);
        assert!(
            (0..1000).contains(&rel),
            "expected rel in [0, 1000), got {}",
            rel
        );
        assert!(s.is_in_bidding_window(0));
    }

    #[test]
    fn test_ms_relative_to_slot_post_window() {
        // slot 0 started 5s ago rel is ~+5000, outside [-1000, +1000).
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let s = make_scheduler(now_secs.saturating_sub(5));
        assert!(!s.is_in_bidding_window(0));
    }
}
