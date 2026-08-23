//! `NapiWaker` — IRQ-safe edge-triggered wake primitive for kernel I/O kthreads.
//!
//! An `AtomicBool` edge flag paired with a [`slopos_ostd::sync::WaitQueue`]: the
//! IRQ side stores the flag and `wake_all`s, the kthread blocks on a predicate
//! that consumes it. The predicate runs after the queue's race-close, so an IRQ
//! arriving between the decision to block and descheduling is seen on recheck.

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_ostd::sync::LockClassKey;
use slopos_ostd::sync::kernel_io_task::{KernelIoStop, KernelIoToken, KthreadWait};

/// IRQ-safe edge-triggered wake primitive.
///
/// Each [`wait`] consumes one armed edge, so multiple [`arm_and_wake`] calls
/// between waits coalesce into a single wake.
pub struct NapiWaker {
    /// Held outside the `WaitQueue` so the IRQ side can publish an edge with no
    /// waiter parked.
    armed: AtomicBool,
    stop: KernelIoStop,
}

impl NapiWaker {
    /// `name` identifies the parked thread in the shutdown report.
    pub const fn new(name: &'static str, class: &'static LockClassKey) -> Self {
        Self {
            armed: AtomicBool::new(false),
            stop: KernelIoStop::new(name, class),
        }
    }

    /// The stop signal this waker parks on; register it so shutdown can reach
    /// the thread.
    #[inline]
    pub const fn stop(&self) -> &KernelIoStop {
        &self.stop
    }

    #[inline]
    fn consume_edge(&self) -> bool {
        self.armed
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// **IRQ-safe.** With no waiter — or under a freeze, which `wake_for_work`
    /// declines to disturb — the edge stays set and the next wait returns.
    #[inline]
    pub fn arm_and_wake(&self) {
        self.armed.store(true, Ordering::Release);
        self.stop.wake_for_work();
    }

    /// Park the calling kthread until an [`arm_and_wake`] runs or a stop is
    /// requested.
    pub fn wait(&self, token: &KernelIoToken<'_>) -> KthreadWait {
        token.park(&self.stop, || self.consume_edge())
    }

    /// Park for at most `timeout_ms` milliseconds.
    pub fn wait_timeout_ms(&self, token: &KernelIoToken<'_>, timeout_ms: u32) -> KthreadWait {
        token.park_timeout(&self.stop, || self.consume_edge(), timeout_ms as u64)
    }

    /// Force-arm without waking, for a post-burst recheck that found work an
    /// IRQ raced in before the kthread re-entered `wait`.
    #[inline]
    pub fn rearm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    /// Test hook: observe the armed-edge handoff without a [`KernelIoToken`],
    /// which only the kthread spawn trampoline can mint.
    #[cfg(feature = "test-hooks")]
    pub fn consume_edge_for_test(&self) -> bool {
        self.consume_edge()
    }
}
