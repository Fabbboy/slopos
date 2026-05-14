//! Per-task ring buffer for `SYSCALL_TEST_REPORT` payloads.
//!
//! A user task's first `SYSCALL_TEST_REPORT` lazily allocates one of these
//! rings into `Task::test_reports`. Non-test tasks never call the syscall
//! and pay zero cost. After the task exits, the kernel-side userland-test
//! runner calls `task_drain_test_reports` (in the kernel-side scheduler)
//! to take ownership of the ring and read out the recorded subtests.

use slopos_abi::syscall::{TEST_REPORT_MSG_MAX, TEST_REPORT_NAME_MAX, TEST_REPORT_RING_CAPACITY};
use slopos_abi::task::INVALID_TASK_ID;

use crate::sync::{LOCK_LEVEL_REGISTRY, SpinLock};
use crate::{AllocError, KBox, KVec, Zeroable};

/// One subtest result. `name`/`msg` are length-prefixed byte arrays — the
/// `*_len` fields hold the populated prefix length; the remainder is zero.
#[derive(Clone, Copy, Zeroable)]
#[repr(C)]
pub struct TestReport {
    pub status: u8,
    pub name_len: u8,
    pub msg_len: u8,
    _pad: u8,
    pub name: [u8; TEST_REPORT_NAME_MAX],
    pub msg: [u8; TEST_REPORT_MSG_MAX],
}

/// Bounded per-task ring of `TestReport`. Newest report is dropped on
/// overflow and the `overflow` flag is latched so the runner can flag
/// truncation in its KTAP output.
#[derive(Zeroable)]
#[repr(C)]
pub struct TestReportRing {
    count: u16,
    overflow: u8,
    _pad: u8,
    entries: [TestReport; TEST_REPORT_RING_CAPACITY],
}

impl TestReportRing {
    pub fn push(&mut self, r: TestReport) {
        let cap = self.entries.len();
        let idx = self.count as usize;
        if idx >= cap {
            self.overflow = 1;
            return;
        }
        self.entries[idx] = r;
        self.count += 1;
    }

    /// Move every recorded `TestReport` out of the ring into a heap vector,
    /// resetting the ring to empty in place.
    pub fn drain(&mut self) -> Result<KVec<TestReport>, AllocError> {
        let mut out: KVec<TestReport> = KVec::new();
        for i in 0..self.count as usize {
            out.push(self.entries[i])?;
        }
        self.count = 0;
        self.overflow = 0;
        Ok(out)
    }

    pub fn overflow_flag(&self) -> bool {
        self.overflow != 0
    }
}

/// Heap-allocate a fresh zeroed ring. Uses in-place init so the ~12 KiB
/// `TestReportRing` rvalue never lands on the caller's stack — required to
/// stay under the 2 KiB stack-frame gate.
pub fn alloc_ring() -> Result<KBox<TestReportRing>, AllocError> {
    KBox::<TestReportRing>::zeroed()
}

/// Construct an empty `TestReport` so users don't need to spell out the
/// padding byte. Used by the syscall handler when copying name/msg buffers.
pub fn empty_report() -> TestReport {
    TestReport {
        status: 0,
        name_len: 0,
        msg_len: 0,
        _pad: 0,
        name: [0; TEST_REPORT_NAME_MAX],
        msg: [0; TEST_REPORT_MSG_MAX],
    }
}

// =============================================================================
// PendingDrain — slot-lifecycle-independent stash of test-task termination
// =============================================================================
//
// The per-task `TestReportRing` lives in the `Task::test_reports` field and
// is tied to the slot's lifecycle: tier-2 reuse via `reserve_task_slot` calls
// `Task::reset_in_place`, which drops the ring — so the report data the
// userland-test runner needs to drain disappears the moment the slot is
// recycled.
//
// `PendingDrain` decouples the test framework from the slot lifecycle: when a
// task that has lazily allocated a `TestReportRing` terminates, the kernel
// moves the ring into a process-wide map keyed by the original `task_id`. The
// userland-test runner reads (and removes) from this map by `task_id`, so it
// doesn't matter whether the slot has been recycled, reset, or even
// reassigned to a different task in between.
//
// Exit info is no longer stashed here — `Task::exit_info` is the single
// source of truth, kept stable across the Zombie state until `waitpid` (or
// the parent's own termination) reaps the slot.
//
// The map is sized for the handful of in-flight test entries the harness
// produces (one at a time today, ≤ 256 in pathological future configurations);
// `MAX_PENDING_DRAINS` caps it so a runaway producer can't unbounded-grow the
// kernel heap. Entries that aren't consumed by a runner (e.g. a test crash
// that bypasses the dispatch path) leak until the next boot — that's
// acceptable for the kernel-test scale. Non-test tasks (those without a
// lazily-allocated ring) never produce an entry; the cache stays empty for
// production boots that disable the harness entirely.

/// Soft cap on simultaneous live drain entries. Real workloads emit one entry
/// at a time; the limit exists only as a runaway-producer guard. Hitting it
/// causes the eldest entry to be evicted (which the dispatch path will then
/// observe as a "no drain entry" Fail).
const MAX_PENDING_DRAINS: usize = 256;

/// Single drain entry. `reports` is `None` only for tasks that never managed
/// to invoke `SYSCALL_TEST_REPORT` — useful for distinguishing "binary
/// crashed before reporting" from "binary reported nothing intentionally" at
/// the runner-side roll-up.
pub struct PendingDrain {
    pub reports: Option<KBox<TestReportRing>>,
}

struct PendingSlot {
    task_id: u32,
    drain: PendingDrain,
}

struct PendingDrainTable {
    entries: KVec<PendingSlot>,
}

impl PendingDrainTable {
    const fn new() -> Self {
        Self {
            entries: KVec::new(),
        }
    }

    fn insert(&mut self, task_id: u32, drain: PendingDrain) {
        for slot in self.entries.iter_mut() {
            if slot.task_id == task_id {
                slot.drain = drain;
                return;
            }
        }
        // Soft cap: drop the eldest entry if the cap is hit. We deliberately
        // discard the OLDEST entry (index 0) rather than the new one because
        // the new termination is what the live runner is currently waiting
        // on; an old un-consumed entry is by definition orphaned.
        if self.entries.len() >= MAX_PENDING_DRAINS {
            // `swap_remove(0)` swaps the last entry into position 0 and pops.
            let _ = self.entries.swap_remove(0);
        }
        let _ = self.entries.push(PendingSlot { task_id, drain });
    }

    fn remove(&mut self, task_id: u32) -> Option<PendingDrain> {
        let pos = self.entries.iter().position(|s| s.task_id == task_id)?;
        let removed = self.entries.swap_remove(pos);
        Some(removed.drain)
    }

    fn contains(&self, task_id: u32) -> bool {
        self.entries.iter().any(|s| s.task_id == task_id)
    }
}

static PENDING_DRAINS: SpinLock<PendingDrainTable> =
    SpinLock::new(PendingDrainTable::new(), LOCK_LEVEL_REGISTRY);

/// Stash a terminated task's (optional) test-report ring into the
/// pending-drain cache. Called from `mark_task_terminated` BEFORE
/// `release_task_dependents` wakes any waiter.
///
/// The ordering is load-bearing: the runner-side wait completes via
/// `release_task_dependents` (or via `task_get_info` returning `Invalid` once
/// the slot is reset). In either case, by the time the runner reaches
/// `consume_pending_drain`, this insert has already happened.
///
/// `task_id` must match the value the caller will eventually look up from —
/// always the terminating task's own `task_id` at the moment of termination.
pub fn stash_pending_drain(task_id: u32, drain: PendingDrain) {
    if task_id == INVALID_TASK_ID {
        return;
    }
    PENDING_DRAINS.lock().insert(task_id, drain);
}

/// Atomically take the drain entry for `task_id`, removing it from the cache.
/// Returns `None` if no entry exists (task wasn't a test task, terminated
/// without going through `mark_task_terminated`, or the entry has already
/// been consumed).
pub fn consume_pending_drain(task_id: u32) -> Option<PendingDrain> {
    PENDING_DRAINS.lock().remove(task_id)
}

/// Non-destructively check whether a drain entry is present. Used by the
/// runner to short-circuit the wait in the rare case where the child
/// terminated before the runner reached `task_wait_for`.
pub fn pending_drain_present(task_id: u32) -> bool {
    PENDING_DRAINS.lock().contains(task_id)
}
