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
//!
//! An optional `ReplaceEventObserver` may be attached at construction. It is
//! notified inline from `add_event` for every successful (non-stale) store.

use std::{
    collections::VecDeque,
    hash::Hash,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
};

use ahash::{HashMap, HashSet};
use parking_lot::RwLock;
use tokio::sync::Notify;

type SharedReplaceEventScheduledEvents<K, V> = Arc<RwLock<ReplaceEventScheduledEvents<K, V>>>;

/// Inline notification hook called from [`ReplaceEventScheduler::add_event`]
/// for every successful (non-stale) store. Implementations must be quick;
/// they run while the scheduler holds its inner write lock.
pub trait ReplaceEventObserver<K, V>: Send + Sync {
    fn on_event(&self, key: &K, seq: u64, value: &V);
}

/// No-op observer used when the caller doesn't need observation.
impl<K, V> ReplaceEventObserver<K, V> for () {
    fn on_event(&self, _key: &K, _seq: u64, _value: &V) {}
}

impl<K, V, T> ReplaceEventObserver<K, V> for Arc<T>
where
    T: ReplaceEventObserver<K, V> + ?Sized,
{
    fn on_event(&self, key: &K, seq: u64, value: &V) {
        (**self).on_event(key, seq, value);
    }
}

#[derive(Debug, Clone)]
pub struct ReplaceEventScheduler<K, V, O = ()> {
    inner: SharedReplaceEventScheduledEvents<K, V>,
    /// Subscriptions are held as weak references so a dropped
    /// [`ReplaceEventSchedulerSubscription`] is reclaimed automatically.
    /// [`add_event`](Self::add_event) upgrades each weak before pushing; if any
    /// upgrade fails, [`Self::needs_subscription_cleanup`] is set so the next
    /// `add_event` (or `subscribe`) compacts the list under a write lock.
    subscriptions: Arc<RwLock<Vec<SubscriptionEntry<K>>>>,
    needs_subscription_cleanup: Arc<AtomicBool>,
    observer: O,
}

#[derive(Debug)]
struct SubscriptionEntry<K> {
    inner: Weak<SubscriptionInner<K>>,
}

#[derive(Debug)]
struct SubscriptionInner<K> {
    unprocessed: RwLock<ReplaceEventUnprocessedEvents<K>>,
    notify: Notify,
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

impl<K, V> ReplaceEventScheduler<K, V, ()> {
    pub fn new() -> Self {
        Self::with_observer(())
    }
}

impl<K, V, O> ReplaceEventScheduler<K, V, O> {
    pub fn with_observer(observer: O) -> Self {
        Self {
            inner: Arc::new(RwLock::new(ReplaceEventScheduledEvents {
                data: HashMap::default(),
            })),
            subscriptions: Arc::new(RwLock::new(Vec::new())),
            needs_subscription_cleanup: Arc::new(AtomicBool::new(false)),
            observer,
        }
    }
}

impl<K, V, O> ReplaceEventScheduler<K, V, O>
where
    K: Hash + Eq + Clone,
    O: ReplaceEventObserver<K, V>,
{
    /// Registers a new subscription seeded with every key currently in
    /// storage, so the worker observes the existing state plus all
    /// subsequent updates.
    pub fn subscribe(&self) -> ReplaceEventSchedulerSubscription<K, V> {
        let mut subscriptions = self.subscriptions.write();
        if self
            .needs_subscription_cleanup
            .swap(false, Ordering::Acquire)
        {
            subscriptions.retain(|entry| entry.inner.strong_count() > 0);
        }
        let scheduler_inner = self.inner.read();
        let mut unprocessed_set = HashSet::default();
        let mut unprocessed_queue = VecDeque::with_capacity(scheduler_inner.data.len());
        for key in scheduler_inner.data.keys() {
            unprocessed_set.insert(key.clone());
            unprocessed_queue.push_back(key.clone());
        }
        let inner = Arc::new(SubscriptionInner {
            unprocessed: RwLock::new(ReplaceEventUnprocessedEvents {
                unprocessed_set,
                unprocessed_queue,
            }),
            notify: Notify::new(),
        });
        // Seed permit so the first notified() returns immediately if anything was already pending.
        if !scheduler_inner.data.is_empty() {
            inner.notify.notify_one();
        }
        subscriptions.push(SubscriptionEntry {
            inner: Arc::downgrade(&inner),
        });
        ReplaceEventSchedulerSubscription {
            inner,
            events: self.inner.clone(),
        }
    }

    /// Stores `event` under `key` if `seq` is greater than or equal to the
    /// sequence number already stored for that key (or no entry exists yet),
    /// and notifies every subscription. Stale events (seq strictly less than
    /// the stored one) are dropped silently. Same-seq adds replace the stored
    /// value — used by callers (e.g. eviction emitters) that need to overwrite
    /// an entry while reusing its existing seq.
    ///
    /// On a non-stale outcome the attached observer's `on_event` is called
    /// with the same `(key, seq, value)` — observers see every successful
    /// store, including intermediates that subscribers may coalesce away.
    ///
    /// If a subscription already has the same key pending, the key is not
    /// queued again — the worker will see only the latest value when it pops.
    pub fn add_event(&self, key: K, seq: u64, event: V) -> AddEventOutcome<V> {
        if self
            .needs_subscription_cleanup
            .swap(false, Ordering::Acquire)
        {
            let mut subs = self.subscriptions.write();
            subs.retain(|entry| entry.inner.strong_count() > 0);
        }
        let subscriptions = self.subscriptions.read();
        let mut inner = self.inner.write();
        let outcome = match inner.data.get(&key) {
            Some(entry) if entry.seq > seq => return AddEventOutcome::Stale,
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
        // Observe before subscribers so the journaled order matches what the producer saw.
        let stored = inner.data.get(&key).expect("just inserted");
        self.observer.on_event(&key, stored.seq, &stored.value);
        let mut saw_dead = false;
        for subscription in subscriptions.iter() {
            let Some(sub_inner) = subscription.inner.upgrade() else {
                saw_dead = true;
                continue;
            };
            let mut sub = sub_inner.unprocessed.write();
            if sub.unprocessed_set.insert(key.clone()) {
                sub.unprocessed_queue.push_back(key.clone());
            }
            sub_inner.notify.notify_one();
        }
        if saw_dead {
            self.needs_subscription_cleanup
                .store(true, Ordering::Release);
        }
        outcome
    }
}

impl<K, V> Default for ReplaceEventScheduler<K, V, ()> {
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
    inner: Arc<SubscriptionInner<K>>,
    events: SharedReplaceEventScheduledEvents<K, V>,
}

impl<K, V> ReplaceEventSchedulerSubscription<K, V>
where
    K: Hash + Eq,
    V: Clone,
{
    /// Pops up to `max_events` pending events into `output` and returns the
    /// number appended. Each event is paired with the latest `(seq, value)`
    /// currently stored for its key, so stale intermediate values are skipped.
    pub fn pop_unprocessed_events(
        &self,
        max_events: usize,
        output: &mut Vec<(K, u64, V)>,
    ) -> usize {
        let events = self.events.read();
        let mut state = self.inner.unprocessed.write();
        let mut count = 0;
        while count < max_events {
            let Some(key) = state.unprocessed_queue.pop_front() else {
                break;
            };
            state.unprocessed_set.remove(&key);
            if let Some(entry) = events.data.get(&key) {
                output.push((key, entry.seq, entry.value.clone()));
                count += 1;
            }
        }
        count
    }

    /// Awaits the next subscription update.
    pub async fn notified(&self) {
        self.inner.notify.notified().await;
    }
}

#[derive(Debug)]
struct ReplaceEventUnprocessedEvents<K> {
    unprocessed_set: HashSet<K>,
    unprocessed_queue: VecDeque<K>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    fn pop_all(sub: &ReplaceEventSchedulerSubscription<u32, u32>) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        sub.pop_unprocessed_events(usize::MAX, &mut out);
        out.into_iter().map(|(k, _, v)| (k, v)).collect()
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
        // same-seq replaces (used by eviction emitters that reuse the existing seq)
        assert_eq!(scheduler.add_event(1, 5, 20), AddEventOutcome::Replaced(10));
        // strictly-lower seq is stale
        assert_eq!(scheduler.add_event(1, 4, 30), AddEventOutcome::Stale);
        assert_eq!(scheduler.add_event(1, 6, 40), AddEventOutcome::Replaced(20));
    }

    #[test]
    fn stale_add_does_not_notify_subscribers() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        scheduler.add_event(1, 5, 10);
        let sub = scheduler.subscribe();
        assert_eq!(pop_all(&sub), vec![(1, 10)]);
        // strictly-lower seq is stale and does not notify
        assert_eq!(scheduler.add_event(1, 4, 99), AddEventOutcome::Stale);
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
    fn pop_returns_seq_alongside_value() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        let sub = scheduler.subscribe();
        scheduler.add_event(1, 7, 10);
        scheduler.add_event(2, 9, 20);
        let mut out = Vec::new();
        sub.pop_unprocessed_events(usize::MAX, &mut out);
        assert_eq!(out, vec![(1, 7, 10), (2, 9, 20)]);
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
    fn dropped_subscription_is_reclaimed_on_next_add() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        let sub_alive = scheduler.subscribe();
        {
            let _sub_dead = scheduler.subscribe();
        }
        assert_eq!(scheduler.subscriptions.read().len(), 2);
        assert!(!scheduler.needs_subscription_cleanup.load(Ordering::Relaxed));

        scheduler.add_event(1, 1, 10);
        assert!(scheduler.needs_subscription_cleanup.load(Ordering::Relaxed));
        assert_eq!(scheduler.subscriptions.read().len(), 2);

        scheduler.add_event(2, 1, 20);
        assert_eq!(scheduler.subscriptions.read().len(), 1);
        assert!(!scheduler.needs_subscription_cleanup.load(Ordering::Relaxed));

        assert_eq!(pop_all(&sub_alive), vec![(1, 10), (2, 20)]);
    }

    #[test]
    fn dropped_subscription_is_reclaimed_on_next_subscribe() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        {
            let _sub_dead = scheduler.subscribe();
        }
        scheduler.add_event(1, 1, 10);
        assert!(scheduler.needs_subscription_cleanup.load(Ordering::Relaxed));
        assert_eq!(scheduler.subscriptions.read().len(), 1);

        let _sub_new = scheduler.subscribe();
        assert_eq!(scheduler.subscriptions.read().len(), 1);
        assert!(!scheduler.needs_subscription_cleanup.load(Ordering::Relaxed));
    }

    #[test]
    fn add_after_subscribe_overrides_seeded_value() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        scheduler.add_event(1, 1, 10);
        let sub = scheduler.subscribe();
        scheduler.add_event(1, 2, 99);
        assert_eq!(pop_all(&sub), vec![(1, 99)]);
    }

    #[derive(Debug, Default)]
    struct RecordingObserver {
        events: Mutex<Vec<(u32, u64, u32)>>,
    }

    impl ReplaceEventObserver<u32, u32> for RecordingObserver {
        fn on_event(&self, key: &u32, seq: u64, value: &u32) {
            self.events.lock().push((*key, seq, *value));
        }
    }

    #[test]
    fn observer_sees_every_non_stale_add_with_seq() {
        let observer = Arc::new(RecordingObserver::default());
        let scheduler = ReplaceEventScheduler::<u32, u32, _>::with_observer(Arc::clone(&observer));
        scheduler.add_event(1, 1, 10);
        scheduler.add_event(1, 2, 20);
        scheduler.add_event(1, 2, 30); // same-seq replace — observer sees it
        scheduler.add_event(1, 1, 99); // strictly-lower seq is stale — observer does not see it
        scheduler.add_event(2, 5, 40);
        let recorded = observer.events.lock().clone();
        assert_eq!(
            recorded,
            vec![(1, 1, 10), (1, 2, 20), (1, 2, 30), (2, 5, 40)]
        );
    }

    #[tokio::test]
    async fn notified_wakes_on_add_and_does_not_lose_permits() {
        let scheduler = ReplaceEventScheduler::<u32, u32>::new();
        let sub = scheduler.subscribe();
        let waiter = tokio::spawn(async move {
            sub.notified().await;
            pop_all(&sub)
        });
        scheduler.add_event(1, 1, 10);
        let drained = waiter.await.unwrap();
        assert_eq!(drained, vec![(1, 10)]);

        let sub2 = scheduler.subscribe();
        let _ = pop_all(&sub2);
        scheduler.add_event(2, 1, 20);
        sub2.notified().await;
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
            for &(_, _, v) in &buf {
                all.push(v);
            }
            if producer_done.load(Ordering::Acquire) {
                buf.clear();
                sub.pop_unprocessed_events(100, &mut buf);
                for &(_, _, v) in &buf {
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
