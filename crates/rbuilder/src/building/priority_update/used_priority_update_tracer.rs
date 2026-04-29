//! Pair of entities used together: a revm [`Inspector`] that tracks the call
//! frame tree, and a [`UsedPriorityStateStorageTracker`] that records reads
//! of registered slots. Classify how each slot was used by the executed
//! bundle:
//!
//! - [`UsedStorageSlotStatus::Unread`] — slot was never read.
//! - [`UsedStorageSlotStatus::Read`] — slot was read and the call stack from
//!   the read point up to the root committed without revert.
//! - [`UsedStorageSlotStatus::ReadReverted`] — slot was read but at least one
//!   ancestor frame on the call stack at the time of the read reverted.

use ahash::{HashMap, HashSet};
use alloy_primitives::B256;
use parking_lot::Mutex;
use rbuilder_primitives::evm_inspector::SlotKey;
use revm::{
    bytecode::opcode,
    interpreter::{
        interpreter_types::{InputsTr, Jumps},
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter,
    },
    Inspector,
};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsedStorageSlotStatus {
    Unread,
    Read,
    ReadReverted,
}

#[derive(Debug)]
struct UsedPriorityStateInner {
    slots_to_track: HashSet<SlotKey>,
    /// Every (slot, frame id) at which a tracked slot was read. A slot may be
    /// read at multiple frames over the course of execution (e.g. once inside
    /// a call that reverts and again at the top level); a slot classifies as
    /// [`UsedStorageSlotStatus::Read`] iff at least one such frame ends up
    /// committing.
    slots_read: HashSet<(SlotKey, usize)>,
    /// Frame ids of currently-active frames; the back is the executing frame.
    frame_stack: Vec<usize>,
    next_frame_id: usize,
    /// Frames that completed without revert. When frame `k` reverts, every id
    /// `>= k` in this set is by construction one of `k`'s descendants — they
    /// were assigned ids after `k` entered and have already left the stack by
    /// the time `k` exits — so they can be discarded wholesale.
    successful_frame_ids: HashSet<usize>,
}

impl UsedPriorityStateInner {
    fn new(slots_to_track: HashSet<SlotKey>) -> Self {
        Self {
            slots_to_track,
            slots_read: HashSet::default(),
            frame_stack: Vec::new(),
            next_frame_id: 0,
            successful_frame_ids: HashSet::default(),
        }
    }

    fn enter_frame(&mut self) {
        let frame_id = self.next_frame_id;
        self.next_frame_id += 1;
        self.frame_stack.push(frame_id);
    }

    fn leave_frame(&mut self, succeeded: bool) {
        let frame_id = self
            .frame_stack
            .pop()
            .expect("leave_frame without matching enter_frame");
        if succeeded {
            self.successful_frame_ids.insert(frame_id);
        } else {
            self.successful_frame_ids.retain(|id| *id < frame_id);
        }
    }

    fn record_storage_read(&mut self, slot: SlotKey) {
        if !self.slots_to_track.contains(&slot) {
            return;
        }
        let &frame_id = self
            .frame_stack
            .last()
            .expect("storage read outside of any call frame");
        self.slots_read.insert((slot, frame_id));
    }
}

/// Inspector half of the tracer pair. Tracks the call-frame tree and which
/// frames committed.
#[derive(Clone, Debug)]
pub struct UsedPriorityStateTracer {
    inner: Arc<Mutex<UsedPriorityStateInner>>,
}

/// Storage-side half of the tracer pair: registers slots for tracking and
/// records reads of them against the currently-executing EVM call frame.
/// Use this when you have a [`revm::Database`] layer (e.g. an overlay) that
/// already intercepts reads and want to feed them into the tracker without
/// wrapping the database itself.
#[derive(Clone, Debug)]
pub struct UsedPriorityStateStorageTracker {
    inner: Arc<Mutex<UsedPriorityStateInner>>,
}

impl UsedPriorityStateStorageTracker {
    /// Add `slot` to the tracker's slot-set so future reads of it are recorded.
    pub fn track_slot(&self, slot: SlotKey) {
        self.inner.lock().slots_to_track.insert(slot);
    }

    /// Record a read of `slot` at the currently-executing call frame. No-op
    /// if `slot` is not in the tracker's slot-set.
    pub fn record_storage_read(&self, slot: SlotKey) {
        self.inner.lock().record_storage_read(slot);
    }
}

#[cfg(test)]
impl UsedPriorityStateTracer {
    pub(super) fn test_enter_frame(&self) {
        self.inner.lock().enter_frame();
    }

    pub(super) fn test_leave_frame(&self, succeeded: bool) {
        self.inner.lock().leave_frame(succeeded);
    }
}

impl UsedPriorityStateTracer {
    pub fn new(slots_to_track: impl IntoIterator<Item = SlotKey>) -> Self {
        let mut set: HashSet<SlotKey> = HashSet::default();
        set.extend(slots_to_track);
        Self {
            inner: Arc::new(Mutex::new(UsedPriorityStateInner::new(set))),
        }
    }

    /// Returns a storage tracker sharing state with this inspector. Use it
    /// when you already have a database layer that intercepts reads and just
    /// want to feed slots/reads into the tracker.
    pub fn storage_tracker(&self) -> UsedPriorityStateStorageTracker {
        UsedPriorityStateStorageTracker {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Returns the per-slot classification for every tracked slot.
    ///
    /// Call this after EVM execution has finished. Frames still on the stack
    /// at call time are treated as not-yet-committed (reads tagged at them
    /// will classify as [`UsedStorageSlotStatus::ReadReverted`]).
    pub fn classification(&self) -> HashMap<SlotKey, UsedStorageSlotStatus> {
        let inner = self.inner.lock();
        assert!(
            inner.frame_stack.is_empty(),
            "classification called before all frames have exited"
        );
        let mut out: HashMap<SlotKey, UsedStorageSlotStatus> =
            HashMap::with_capacity_and_hasher(inner.slots_to_track.len(), Default::default());
        for slot in &inner.slots_to_track {
            out.insert(slot.clone(), UsedStorageSlotStatus::Unread);
        }
        for (slot, frame_id) in &inner.slots_read {
            let Some(status) = out.get_mut(slot) else {
                continue;
            };
            if matches!(status, UsedStorageSlotStatus::Read) {
                continue;
            }
            *status = if inner.successful_frame_ids.contains(frame_id) {
                UsedStorageSlotStatus::Read
            } else {
                UsedStorageSlotStatus::ReadReverted
            };
        }
        out
    }
}

impl<CTX> Inspector<CTX> for UsedPriorityStateTracer {
    fn step(&mut self, interpreter: &mut Interpreter, _context: &mut CTX) {
        if interpreter.bytecode.opcode() == opcode::SLOAD {
            if let Ok(key) = interpreter.stack.peek(0) {
                self.inner.lock().record_storage_read(SlotKey {
                    address: interpreter.input.target_address(),
                    key: B256::from(key.to_be_bytes()),
                });
            }
        }
    }

    fn call(&mut self, _ctx: &mut CTX, _inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.inner.lock().enter_frame();
        None
    }

    fn call_end(&mut self, _ctx: &mut CTX, _inputs: &CallInputs, outcome: &mut CallOutcome) {
        let succeeded = outcome.instruction_result().is_ok();
        self.inner.lock().leave_frame(succeeded);
    }

    fn create(&mut self, _ctx: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.inner.lock().enter_frame();
        None
    }

    fn create_end(&mut self, _ctx: &mut CTX, _inputs: &CreateInputs, outcome: &mut CreateOutcome) {
        let succeeded = outcome.instruction_result().is_ok();
        self.inner.lock().leave_frame(succeeded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::test_utils::{addr, hash};
    use alloy_primitives::Address;

    fn slot(addr_id: u64, key_id: u64) -> SlotKey {
        SlotKey {
            address: addr(addr_id),
            key: hash(key_id),
        }
    }

    /// Test harness driving the inner state directly. Mirrors the EVM call
    /// stack: `new_frame(addr)` enters a frame and remembers the executing
    /// contract so `read_slot(key)` can compose a [`SlotKey`] for it.
    struct Tester {
        tracer: UsedPriorityStateTracer,
        addr_stack: Vec<Address>,
    }

    impl Tester {
        fn new<I>(slots: I) -> Self
        where
            I: IntoIterator<Item = SlotKey>,
        {
            Self {
                tracer: UsedPriorityStateTracer::new(slots),
                addr_stack: Vec::new(),
            }
        }

        fn new_frame(&mut self, address: Address) {
            self.tracer.inner.lock().enter_frame();
            self.addr_stack.push(address);
        }

        fn read_slot(&mut self, key: u64) {
            let address = *self
                .addr_stack
                .last()
                .expect("read_slot called outside any frame");
            self.tracer.inner.lock().record_storage_read(SlotKey {
                address,
                key: hash(key),
            });
        }

        fn exit_frame(&mut self, ok: bool) {
            self.tracer.inner.lock().leave_frame(ok);
            self.addr_stack.pop();
        }

        fn get_result(&self) -> HashMap<SlotKey, UsedStorageSlotStatus> {
            self.tracer.classification()
        }
    }

    #[test]
    fn slot_never_read_is_unread() {
        let s = slot(1, 1);
        let mut t = Tester::new([s.clone()]);
        t.new_frame(addr(1));
        t.exit_frame(true);
        assert_eq!(t.get_result()[&s], UsedStorageSlotStatus::Unread);
    }

    #[test]
    fn read_in_committed_root_frame_is_read() {
        let s = slot(1, 1);
        let mut t = Tester::new([s.clone()]);
        t.new_frame(addr(1));
        t.read_slot(1);
        t.exit_frame(true);
        assert_eq!(t.get_result()[&s], UsedStorageSlotStatus::Read);
    }

    #[test]
    fn read_in_reverted_root_frame_is_revert() {
        let s = slot(1, 1);
        let mut t = Tester::new([s.clone()]);
        t.new_frame(addr(1));
        t.read_slot(1);
        t.exit_frame(false);
        assert_eq!(t.get_result()[&s], UsedStorageSlotStatus::ReadReverted);
    }

    #[test]
    fn read_in_reverted_subcall_is_revert() {
        // root commits but the inner subcall (which did the read) reverts
        let s = slot(1, 1);
        let mut t = Tester::new([s.clone()]);
        t.new_frame(addr(1));
        t.new_frame(addr(1));
        t.read_slot(1);
        t.exit_frame(false);
        t.exit_frame(true);
        assert_eq!(t.get_result()[&s], UsedStorageSlotStatus::ReadReverted);
    }

    #[test]
    fn committed_subcall_under_reverted_parent_is_revert() {
        // inner subcall commits, parent reverts → all subcall reads vanish
        let s = slot(1, 1);
        let mut t = Tester::new([s.clone()]);
        t.new_frame(addr(1));
        t.new_frame(addr(1));
        t.read_slot(1);
        t.exit_frame(true);
        t.exit_frame(false);
        assert_eq!(t.get_result()[&s], UsedStorageSlotStatus::ReadReverted);
    }

    #[test]
    fn read_in_reverted_then_committed_frame_is_read() {
        // slot read once inside a reverting subcall and again at top level
        // (which commits) → the second read counts → Read
        let s = slot(1, 1);
        let mut t = Tester::new([s.clone()]);
        t.new_frame(addr(1));
        t.new_frame(addr(1));
        t.read_slot(1);
        t.exit_frame(false);
        t.read_slot(1);
        t.exit_frame(true);
        assert_eq!(t.get_result()[&s], UsedStorageSlotStatus::Read);
    }

    #[test]
    fn deep_revert_invalidates_all_descendants() {
        // A → B → C, A commits, B reverts, C committed; read happens in C
        let s = slot(1, 1);
        let mut t = Tester::new([s.clone()]);
        t.new_frame(addr(1)); // A
        t.new_frame(addr(1)); // B
        t.new_frame(addr(1)); // C
        t.read_slot(1);
        t.exit_frame(true); // C
        t.exit_frame(false); // B
        t.exit_frame(true); // A
        assert_eq!(t.get_result()[&s], UsedStorageSlotStatus::ReadReverted);
    }

    #[test]
    fn sibling_revert_does_not_affect_other_sibling() {
        // A → (B reverts after reading) then A → (C commits after reading)
        // both reads target the same tracked slot
        let s = slot(1, 1);
        let mut t = Tester::new([s.clone()]);
        t.new_frame(addr(1)); // A
        t.new_frame(addr(1)); // B
        t.read_slot(1);
        t.exit_frame(false); // B reverts
        t.new_frame(addr(1)); // C
        t.read_slot(1);
        t.exit_frame(true); // C commits
        t.exit_frame(true); // A commits
        assert_eq!(t.get_result()[&s], UsedStorageSlotStatus::Read);
    }

    #[test]
    fn untracked_slot_is_not_classified() {
        let tracked = slot(1, 1);
        let other = slot(1, 2);
        let mut t = Tester::new([tracked.clone()]);
        t.new_frame(addr(1));
        t.read_slot(2);
        t.exit_frame(true);
        let result = t.get_result();
        assert_eq!(result[&tracked], UsedStorageSlotStatus::Unread);
        assert!(!result.contains_key(&other));
    }

    #[test]
    fn read_per_address_is_isolated() {
        // same key on different addresses are different slots
        let a1 = slot(1, 1);
        let a2 = slot(2, 1);
        let mut t = Tester::new([a1.clone(), a2.clone()]);
        t.new_frame(addr(1));
        t.read_slot(1);
        t.exit_frame(true);
        let result = t.get_result();
        assert_eq!(result[&a1], UsedStorageSlotStatus::Read);
        assert_eq!(result[&a2], UsedStorageSlotStatus::Unread);
    }

    // The tests below simulate a bundle: each top-level call/exit_frame pair
    // is one transaction. Between txs the call stack is empty.

    #[test]
    fn bundle_read_in_first_tx_only() {
        // tx0 reads S and commits; tx1 commits without reading.
        let s = slot(1, 1);
        let mut t = Tester::new([s.clone()]);

        t.new_frame(addr(1));
        t.read_slot(1);
        t.exit_frame(true);

        t.new_frame(addr(1));
        t.exit_frame(true);

        assert_eq!(t.get_result()[&s], UsedStorageSlotStatus::Read);
    }

    #[test]
    fn bundle_read_in_second_tx_only() {
        // tx0 commits without reading; tx1 reads S and commits.
        let s = slot(1, 1);
        let mut t = Tester::new([s.clone()]);

        t.new_frame(addr(1));
        t.exit_frame(true);

        t.new_frame(addr(1));
        t.read_slot(1);
        t.exit_frame(true);

        assert_eq!(t.get_result()[&s], UsedStorageSlotStatus::Read);
    }

    #[test]
    fn bundle_read_only_in_reverted_tx_is_revert() {
        // tx0 reads S then reverts; tx1 commits without touching S.
        let s = slot(1, 1);
        let mut t = Tester::new([s.clone()]);

        t.new_frame(addr(1));
        t.read_slot(1);
        t.exit_frame(false);

        t.new_frame(addr(1));
        t.exit_frame(true);

        assert_eq!(t.get_result()[&s], UsedStorageSlotStatus::ReadReverted);
    }

    #[test]
    fn bundle_read_in_committed_tx_outweighs_revert_in_other_tx() {
        // tx0 reads S and commits; tx1 reads S and reverts. The committed
        // read still counts → Read.
        let s = slot(1, 1);
        let mut t = Tester::new([s.clone()]);

        t.new_frame(addr(1));
        t.read_slot(1);
        t.exit_frame(true);

        t.new_frame(addr(1));
        t.read_slot(1);
        t.exit_frame(false);

        assert_eq!(t.get_result()[&s], UsedStorageSlotStatus::Read);
    }

    #[test]
    fn bundle_per_tx_slots_classified_independently() {
        // tx0 reads S0 and commits; tx1 reads S1 and reverts.
        let s0 = slot(1, 1);
        let s1 = slot(1, 2);
        let mut t = Tester::new([s0.clone(), s1.clone()]);

        t.new_frame(addr(1));
        t.read_slot(1);
        t.exit_frame(true);

        t.new_frame(addr(1));
        t.read_slot(2);
        t.exit_frame(false);

        let result = t.get_result();
        assert_eq!(result[&s0], UsedStorageSlotStatus::Read);
        assert_eq!(result[&s1], UsedStorageSlotStatus::ReadReverted);
    }

    #[test]
    fn bundle_with_nested_calls_per_tx() {
        // tx0: top → sub (revert after reading S0); top exits OK → ReadReverted
        // tx1: top → sub (commit after reading S1); top exits OK → Read
        let s0 = slot(1, 1);
        let s1 = slot(1, 2);
        let mut t = Tester::new([s0.clone(), s1.clone()]);

        t.new_frame(addr(1)); // tx0 top
        t.new_frame(addr(1)); // tx0 sub
        t.read_slot(1);
        t.exit_frame(false); // sub reverts
        t.exit_frame(true); // tx0 commits

        t.new_frame(addr(1)); // tx1 top
        t.new_frame(addr(1)); // tx1 sub
        t.read_slot(2);
        t.exit_frame(true); // sub commits
        t.exit_frame(true); // tx1 commits

        let result = t.get_result();
        assert_eq!(result[&s0], UsedStorageSlotStatus::ReadReverted);
        assert_eq!(result[&s1], UsedStorageSlotStatus::Read);
    }
}
