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
    /// MS into slot to start bidding.
    bid_start_ms: u64,
    /// MS into slot to stop bidding.
    bid_end_ms: u64,
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

    /// Returns ms elapsed since the start of the given slot.
    /// Returns None if the slot hasn't started yet.
    pub fn ms_into_slot(&self, slot: u64) -> Option<u64> {
        let slot_start = self.slot_start_time(slot);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let now_ms = now.as_millis() as u64;
        let slot_start_ms = slot_start * 1000;

        if now_ms >= slot_start_ms {
            Some(now_ms - slot_start_ms)
        } else {
            None
        }
    }

    /// Returns true if we are currently within the biding window for the given slot.
    pub fn is_in_bidding_window(&self, slot: u64) -> bool {
        match self.ms_into_slot(slot) {
            Some(ms) => ms >= self.bid_start_ms && ms < self.bid_end_ms,
            None => false,
        }
    }

    /// Returns the duration until the bidding window opens for the given slot.
    /// Returns Duration::ZERO if the window is already open.
    /// Returns None if the bidding window has already closed.
    pub fn time_until_bid_start(&self, slot: u64) -> Option<Duration> {
        let slot_start_ms = self.slot_start_time(slot) * 1000;
        let bid_open_ms = slot_start_ms + self.bid_start_ms;
        let bid_close_ms = slot_start_ms + self.bid_end_ms;

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        if now_ms >= bid_close_ms {
            // window already closed
            return None;
        }

        if now_ms >= bid_open_ms {
            // window already open
            return Some(Duration::ZERO); 
        }

        Some(Duration::from_millis(bid_open_ms - now_ms))
    }

    /// Returns the duration until the bidding window closes for the given slot.
    /// Returns None if the window has already closed.
    pub fn time_until_bid_end(&self, slot: u64) -> Option<Duration> {
        let slot_start_ms = self.slot_start_time(slot) * 1000;
        let bid_close_ms = slot_start_ms + self.bid_end_ms;

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        if now_ms >= bid_close_ms {
            return None;
        }

        Some(Duration::from_millis(bid_close_ms - now_ms))
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
            bid_start_ms: 0,
            bid_end_ms: 4000,
            bid_interval_ms: 500,
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
        assert_eq!(s.bid_interval(), Some(Duration::from_millis(500)));
    }
}
