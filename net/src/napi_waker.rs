//! `NapiWaker` — IRQ-safe edge-triggered wake primitive for kernel
//! I/O kthreads (Phase-1 scheduler refactor).
//!
//! Composes an `AtomicBool` flag with a [`slopos_ostd::sync::WaitQueue`]:
//!
//! - **Producer side (IRQ context):** [`arm_and_wake`] sets the flag
//!   and `wake_all`s the queue. `WaitQueue::wake_all` is IRQ-safe by
//!   contract (see `slopos-ostd/src/sync/wait_queue.rs:202`: "IRQ
//!   context for `wake_*`"). No scheduler interaction on the IRQ
//!   side — just an atomic store and the queue's intrusive-list
//!   walk.
//!
//! - **Consumer side (kthread context):** [`wait`] blocks via
//!   `WaitQueue::wait_event` with a predicate that
//!   `compare_exchange`s the flag back to false. Idempotent over
//!   multiple signals between waits — the predicate is the
//!   linearisation point.
//!
//! Replaces the pre-refactor pattern of "kthread does
//! `sleep_current_task_ms(1)` in a tight loop and IRQ does
//! `napi_schedule()`". That pattern relied on the scheduler servicing
//! the kthread's sleep deadline promptly — which the regression that
//! prompted the rip-and-replace showed it does not under user-task
//! load (the netpoll task was starved for the full 5 s duration of
//! every `curl` recv wait). `NapiWaker` plus the `KernelIo` priority
//! tier means the kthread parks indefinitely and runs IRQ-driven,
//! exactly when there is work to do.
//!
//! Lost-wakeup safety: the predicate's CAS-style `swap(false)` runs
//! after the queue's race-close lock-pair (see
//! `slopos-ostd/src/sync/wait_queue.rs:548-665`), so an IRQ that
//! arrives between `WaitQueue::wait_event` deciding to block and
//! actually descheduling is observed on the recheck step. Standard
//! `task->WAITQ` pattern.

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_ostd::sync::WaitQueue;

/// IRQ-safe edge-triggered wake primitive.
///
/// `const`-constructible so it can live as a `static`. The owning
/// kthread parks in [`wait`]; one or more IRQs may [`arm_and_wake`];
/// each `wait` call consumes one armed-edge (the predicate is a
/// `swap(false)`, so multiple `arm_and_wake` calls coalesce into one
/// wake, matching Linux NAPI's `napi_schedule` semantics).
pub struct NapiWaker {
    /// Edge-armed flag. Set by `arm_and_wake`; consumed by the
    /// predicate inside `wait`. Stored separately from the
    /// `WaitQueue`'s internal state so the IRQ side can publish a
    /// wake even when no waiter is parked (lost-wakeup avoidance).
    armed: AtomicBool,
    /// The kthread parks here. `wait_event_*` is documented to be
    /// safe to nest with `wake_*` from IRQ context.
    wq: WaitQueue,
}

impl NapiWaker {
    /// `const`-fn constructor so this type can live in a `static`.
    pub const fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            wq: WaitQueue::new(),
        }
    }

    /// **IRQ-safe.** Set the armed flag with `Release` and wake every
    /// task parked on the queue. The `Release` store synchronises
    /// with the `Acquire` `swap(false)` inside the [`wait`]
    /// predicate so the kthread observes the armed edge after wake.
    ///
    /// Safe to call when no waiter is parked — the armed flag stays
    /// set and the next [`wait`] returns immediately.
    #[inline]
    pub fn arm_and_wake(&self) {
        self.armed.store(true, Ordering::Release);
        let _ = self.wq.wake_all();
    }

    /// Park the calling kthread until an [`arm_and_wake`] runs.
    /// Returns when the armed flag is observed true; the predicate
    /// atomically consumes the edge so reentrancy with another
    /// [`arm_and_wake`] mid-wake re-arms naturally.
    pub fn wait(&self) {
        self.wq.wait_event(|| {
            self.armed
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        });
    }

    /// Park for at most `timeout_ms` milliseconds. Returns `true` if
    /// the predicate fired (armed edge consumed), `false` on
    /// timeout. Used by the net-timer kthread which needs to wake
    /// either on `arm_and_wake` (a sooner deadline was scheduled) or
    /// on its computed next-deadline-ms timeout.
    pub fn wait_timeout_ms(&self, timeout_ms: u32) -> bool {
        self.wq.wait_event_timeout(
            || {
                self.armed
                    .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            },
            timeout_ms as u64,
        )
    }

    /// Force-arm without waking. Used after a post-burst recheck
    /// where the IRQ raced us between the last `wait` returning and
    /// the kthread actually re-entering `wait` — set the flag so the
    /// next `wait` returns immediately rather than parking.
    #[inline]
    pub fn rearm(&self) {
        self.armed.store(true, Ordering::Release);
    }
}

impl Default for NapiWaker {
    fn default() -> Self {
        Self::new()
    }
}
