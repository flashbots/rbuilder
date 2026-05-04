//! Keyed event storage with coalescing fan-out to subscribers.
//!
//! Use it when events arrive faster than workers can process them and only
//! the latest value per key matters. Each `add_event` carries a per-key
//! sequence number; an event is stored only if its sequence number is strictly
//! greater than the one currently stored for that key. Subscribers are
//! notified but a key already pending delivery is not re-queued, so workers
//! always see the latest value and skip stale intermediates.
//!
//! New subscriptions are seeded with every key currently in storage, so a
//! freshly-attached worker observes the existing state plus all subsequent
//! updates.

use std::{collections::VecDeque, hash::Hash, sync::Arc};

use ahash::{HashMap, HashSet};
use parking_lot::RwLock;
use tokio::sync::Notify;

type SharedReplaceEventScheduledEvents<K, V> = Arc<RwLock<ReplaceEventScheduledEvents<K, V>>>;
type SharedReplaceEventUnprocessedEvents<K> = Arc<RwLock<ReplaceEventUnprocessedEvents<K>>>;

#[derive(Debug, Clone)]
pub struct ReplaceEventScheduler<K, V> {
    inner: SharedReplaceEventScheduledEvents<K, V>,
    subscriptions: Arc<RwLock<Vec<SubscriptionEntry<K>>>>,
}

#[derive(Debug, Clone)]
struct SubscriptionEntry<K> {
    unprocessed: SharedReplaceEventUnprocessedEvents<K>,
    notify: Arc<Notify>,
}

/// Outcome of [`ReplaceEventScheduler::add_event`].
#[derive(Debug, PartialEq, Eq)]
pub enum AddEventOutcome<V> {
    /// No prior value was stored for this key. The new value is now stored.
    Added,
    /// An existing value was replaced because the new sequence number is
    /// strictly greater. The previous value is returned.
    Replaced(V),
    /// The new sequence number is not strictly greater than the stored one;
    /// the event was dropped and the stored value is unchanged.
    Stale,
}

impl<K, V> ReplaceEventScheduler<K, V> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(ReplaceEventScheduledEvents {
                data: HashMap::default(),
            })),
            subscriptions: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

impl<K, V> ReplaceEventScheduler<K, V>
where
    K: Hash + Eq + Clone,
{
    /// Registers a new subscription seeded with every key currently in
    /// storage, so the worker observes the existing state plus all
    /// subsequent updates.
    pub fn subscribe(&self) -> ReplaceEventSchedulerSubscription<K, V> {
        let mut subscriptions = self.subscriptions.write();
        let inner = self.inner.read();
        let mut unprocessed_set = HashSet::default();
        let mut unprocessed_queue = VecDeque::with_capacity(inner.data.len());
        for key in inner.data.keys() {
            unprocessed_set.insert(key.clone());
            unprocessed_queue.push_back(key.clone());
        }
        let unprocessed = Arc::new(RwLock::new(ReplaceEventUnprocessedEvents {
            unprocessed_set,
            unprocessed_queue,
        }));
        let notify = Arc::new(Notify::new());
        // Seed permit so the first notified() returns immediately if anything was already pending.
        if !inner.data.is_empty() {
            notify.notify_one();
        }
        subscriptions.push(SubscriptionEntry {
            unprocessed: unprocessed.clone(),
            notify: notify.clone(),
        });
        ReplaceEventSchedulerSubscription {
            unprocessed_state: unprocessed,
            events: self.inner.clone(),
            notify,
        }
    }

    /// Stores `event` under `key` if `seq` is strictly greater than the
    /// sequence number already stored for that key (or no entry exists yet),
    /// and notifies every subscription. Stale events (seq not greater than
    /// the stored one) are dropped silently.
    ///
    /// If a subscription already has the same key pending, the key is not
    /// queued again — the worker will see only the latest value when it pops.
    pub fn add_event(&self, key: K, seq: u64, event: V) -> AddEventOutcome<V> {
        let subscriptions = self.subscriptions.read();
        let mut inner = self.inner.write();
        let outcome = match inner.data.get(&key) {
            Some(entry) if entry.seq >= seq => return AddEventOutcome::Stale,
            Some(_) => {
                let prev = inner
                    .data
                    .insert(key.clone(), StoredEvent { seq, value: event })
                    .expect("entry just observed above");
                AddEventOutcome::Replaced(prev.value)
            }
            None => {
                inner
                    .data
                    .insert(key.clone(), StoredEvent { seq, value: event });
                AddEventOutcome::Added
            }
        };
        for subscription in subscriptions.iter() {
            let mut sub = subscription.unprocessed.write();
            if sub.unprocessed_set.insert(key.clone()) {
                sub.unprocessed_queue.push_back(key.clone());
            }
            subscription.notify.notify_one();
        }
        outcome
    }
}

impl<K, V> Default for ReplaceEventScheduler<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct ReplaceEventScheduledEvents<K, V> {
    data: HashMap<K, StoredEvent<V>>,
}

#[derive(Debug)]
struct StoredEvent<V> {
    seq: u64,
    value: V,
}

#[derive(Debug)]
pub struct ReplaceEventSchedulerSubscription<K, V> {
    unprocessed_state: SharedReplaceEventUnprocessedEvents<K>,
    events: SharedReplaceEventScheduledEvents<K, V>,
    notify: Arc<Notify>,
}

impl<K, V> ReplaceEventSchedulerSubscription<K, V>
where
    K: Hash + Eq,
    V: Clone,
{
    /// Pops up to `max_events` pending events into `output` and returns the
    /// number appended. Each event is paired with the latest value currently
    /// stored for its key, so stale intermediate values are skipped.
    pub fn pop_unprocessed_events(&self, max_events: usize, output: &mut Vec<(K, V)>) -> usize {
        let events = self.events.read();
        let mut state = self.unprocessed_state.write();
        let mut count = 0;
        while count < max_events {
            let Some(key) = state.unprocessed_queue.pop_front() else {
                break;
            };
            state.unprocessed_set.remove(&key);
            if let Some(entry) = events.data.get(&key) {
                output.push((key, entry.value.clone()));
                count += 1;
            }
        }
        count
    }

    /// Awaits the next subscription update.
    pub async fn notified(&self) {
        self.notify.notified().await;
    }
}

#[derive(Debug, Clone)]
pub struct ReplaceEventUnprocessedEvents<K> {
    unprocessed_set: HashSet<K>,
    unprocessed_queue: VecDeque<K>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    fn pop_all(sub: &ReplaceEventSchedulerSubscription<u32, u32>) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        sub.pop_unprocessed_events(usize::MAX, &mut out);
        out
    }

    #[test]
    fn add_event_returns_previous_value() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        assert_eq!(scheduler.add_event(1, 1, 10), AddEventOutcome::Added);
        assert_eq!(scheduler.add_event(1, 2, 20), AddEventOutcome::Replaced(10));
        assert_eq!(scheduler.add_event(2, 1, 30), AddEventOutcome::Added);
    }

    #[test]
    fn add_event_rejects_stale_seq() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        assert_eq!(scheduler.add_event(1, 5, 10), AddEventOutcome::Added);
        assert_eq!(scheduler.add_event(1, 5, 20), AddEventOutcome::Stale);
        assert_eq!(scheduler.add_event(1, 4, 30), AddEventOutcome::Stale);
        assert_eq!(scheduler.add_event(1, 6, 40), AddEventOutcome::Replaced(10));
    }

    #[test]
    fn stale_add_does_not_notify_subscribers() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        scheduler.add_event(1, 5, 10);
        let sub = scheduler.subscribe();
        assert_eq!(pop_all(&sub), vec![(1, 10)]);
        assert_eq!(scheduler.add_event(1, 5, 99), AddEventOutcome::Stale);
        let mut out = Vec::new();
        assert_eq!(sub.pop_unprocessed_events(usize::MAX, &mut out), 0);
    }

    #[test]
    fn pop_returns_added_events_in_fifo_order() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        let sub = scheduler.subscribe();
        scheduler.add_event(2, 1, 20);
        scheduler.add_event(1, 1, 10);
        scheduler.add_event(3, 1, 30);
        assert_eq!(pop_all(&sub), vec![(2, 20), (1, 10), (3, 30)]);
    }

    #[test]
    fn pop_coalesces_replacements_to_latest_value() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        let sub = scheduler.subscribe();
        scheduler.add_event(1, 1, 10);
        scheduler.add_event(1, 2, 20);
        scheduler.add_event(1, 3, 30);
        assert_eq!(pop_all(&sub), vec![(1, 30)]);
    }

    #[test]
    fn pop_after_pop_sees_new_value() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        let sub = scheduler.subscribe();
        scheduler.add_event(1, 1, 10);
        assert_eq!(pop_all(&sub), vec![(1, 10)]);
        scheduler.add_event(1, 2, 20);
        assert_eq!(pop_all(&sub), vec![(1, 20)]);
    }

    #[test]
    fn pop_respects_max_events_and_queue_drains_across_calls() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        let sub = scheduler.subscribe();
        for i in 0..10 {
            scheduler.add_event(i, 1, i * 10);
        }
        let mut out = Vec::new();
        assert_eq!(sub.pop_unprocessed_events(3, &mut out), 3);
        assert_eq!(out.len(), 3);
        assert_eq!(sub.pop_unprocessed_events(100, &mut out), 7);
        assert_eq!(out.len(), 10);
    }

    #[test]
    fn pop_on_empty_returns_zero() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        let sub = scheduler.subscribe();
        let mut out = Vec::new();
        assert_eq!(sub.pop_unprocessed_events(10, &mut out), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn each_subscription_receives_independent_copy() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        let sub1 = scheduler.subscribe();
        let sub2 = scheduler.subscribe();
        scheduler.add_event(1, 1, 10);
        scheduler.add_event(2, 1, 20);

        assert_eq!(pop_all(&sub1), vec![(1, 10), (2, 20)]);
        assert_eq!(pop_all(&sub2), vec![(1, 10), (2, 20)]);
    }

    #[test]
    fn subscribe_seeds_with_existing_keys_and_latest_value() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        scheduler.add_event(1, 1, 10);
        scheduler.add_event(2, 1, 20);
        scheduler.add_event(1, 2, 100);

        let sub = scheduler.subscribe();
        let mut out = pop_all(&sub);
        out.sort();
        assert_eq!(out, vec![(1, 100), (2, 20)]);
    }

    #[test]
    fn add_after_subscribe_overrides_seeded_value() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        scheduler.add_event(1, 1, 10);
        let sub = scheduler.subscribe();
        scheduler.add_event(1, 2, 99);
        assert_eq!(pop_all(&sub), vec![(1, 99)]);
    }

    #[tokio::test]
    async fn notified_wakes_on_add_and_does_not_lose_permits() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        let sub = scheduler.subscribe();
        // No prior data — notified should block until something is added.
        let waiter = tokio::spawn(async move {
            sub.notified().await;
            pop_all(&sub)
        });
        scheduler.add_event(1, 1, 10);
        let drained = waiter.await.unwrap();
        assert_eq!(drained, vec![(1, 10)]);

        // Permit deposited before await still wakes us.
        let sub2 = scheduler.subscribe();
        let _ = pop_all(&sub2); // drain seeded
        scheduler.add_event(2, 1, 20);
        sub2.notified().await; // returns immediately via stored permit
        assert_eq!(pop_all(&sub2), vec![(2, 20)]);
    }

    #[test]
    fn concurrent_pop_sees_strictly_monotonic_values() {
        const N: u32 = 50_000;
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        let sub = scheduler.subscribe();
        let producer_done = Arc::new(AtomicBool::new(false));

        let producer = {
            let scheduler = scheduler.clone();
            let producer_done = producer_done.clone();
            thread::spawn(move || {
                for i in 0..N {
                    scheduler.add_event(0, (i + 1) as u64, i);
                }
                producer_done.store(true, Ordering::Release);
            })
        };

        let mut all = Vec::new();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            sub.pop_unprocessed_events(100, &mut buf);
            for &(_, v) in &buf {
                all.push(v);
            }
            if producer_done.load(Ordering::Acquire) {
                buf.clear();
                sub.pop_unprocessed_events(100, &mut buf);
                for &(_, v) in &buf {
                    all.push(v);
                }
                break;
            }
        }
        producer.join().unwrap();

        assert!(
            !all.is_empty(),
            "consumer should observe at least one value"
        );
        for w in all.windows(2) {
            assert!(
                w[0] < w[1],
                "values not strictly monotonic: {} then {}",
                w[0],
                w[1]
            );
        }
        assert_eq!(*all.last().unwrap(), N - 1);
        assert!(
            all.len() < N as usize,
            "coalescing did not collapse any events ({} of {})",
            all.len(),
            N
        );
    }
}
