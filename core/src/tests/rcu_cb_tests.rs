//! Kernel-side tests for RCU deferred-callback reclamation.
//!
//! `call_rcu` only queues. Something else has to invoke the callback, and these
//! tests are what say that something is reachable from a context the kernel
//! actually runs: they never call the drain by hand. A queue whose consumer is
//! unreachable looks exactly like a working one from the producer's side, and
//! leaks every deferred object for the life of the boot.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_kernel_services::platform::timer_poll_delay_ms;
use slopos_ostd::KArc;
use slopos_ostd::sync::RcuArcSlot;
use slopos_testing::TestResult;
use slopos_testing::fail;

/// How long to give the drain before calling it unreachable.
///
/// The drain runs when a CPU finds nothing to dispatch, so the bound is a
/// scheduling property rather than a timer one; this is far longer than the
/// hundreds of microseconds an idle CPU needs, and short enough that a genuine
/// regression fails the suite rather than hanging it.
const RECLAIM_DEADLINE_MS: u32 = 400;

/// Poll interval. A polled delay rather than a sleep: the kernel test phase runs
/// on the BSP before it enters the scheduler, so this task cannot block — which
/// is also what makes the wait a faithful test of a busy CPU 0.
const POLL_INTERVAL_MS: u32 = 10;

/// Drive the RCU drain until `done` holds, or the deadline expires. Returns
/// whether it held.
///
/// A test that queues a callback and then calls the drain once is asserting
/// that *it* performed the invocation, which stops being true the moment an
/// idle CPU drains concurrently: a CPU that has already detached the chain
/// leaves nothing for the manual call to find while the callback is still in
/// flight. Polling for the effect is the assertion that survives.
pub fn drain_until(done: impl Fn() -> bool) -> bool {
    let mut waited = 0;
    loop {
        slopos_ostd::sync::rcu_raise_softirq();
        slopos_ostd::sync::rcu_process_callbacks();
        if done() {
            return true;
        }
        if waited >= RECLAIM_DEADLINE_MS {
            return false;
        }
        timer_poll_delay_ms(POLL_INTERVAL_MS);
        waited += POLL_INTERVAL_MS;
    }
}

static DROPPED: AtomicU32 = AtomicU32::new(0);

struct DropCounted;

impl Drop for DropCounted {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::Release);
    }
}

/// A callback queued by `call_rcu` is invoked without anyone driving the drain.
///
/// This is the reachability test. `RcuArcSlot::store` defers the displaced
/// reference through `call_rcu`, so the payload's `Drop` runs only if the
/// callback does.
pub fn test_rcu_callbacks_are_invoked_without_a_manual_drain() -> TestResult {
    let before = DROPPED.load(Ordering::Acquire);

    let slot = RcuArcSlot::<DropCounted>::empty();
    let Ok(value) = KArc::try_new(DropCounted) else {
        return fail!("KArc allocation failed");
    };
    slot.store(Some(value));
    // Displaces the only reference, which is what reaches `call_rcu`.
    slot.store(None);

    let mut waited = 0;
    while waited < RECLAIM_DEADLINE_MS {
        if DROPPED.load(Ordering::Acquire) != before {
            return TestResult::Pass;
        }
        timer_poll_delay_ms(POLL_INTERVAL_MS);
        waited += POLL_INTERVAL_MS;
    }

    fail!(
        "no RCU callback ran in {}ms — every deferred free since boot is leaking",
        RECLAIM_DEADLINE_MS
    )
}

/// `synchronize_rcu` reaches no allocator.
///
/// It is called from the reclaim path, including `call_rcu`'s own
/// out-of-memory fallback, so an allocation here is a failure exactly when
/// there is nothing to allocate from.
pub fn test_synchronize_rcu_allocates_nothing() -> TestResult {
    let before = slopos_mm::slab::get_heap_stats_owned();
    slopos_ostd::sync::synchronize_rcu();
    let after = slopos_mm::slab::get_heap_stats_owned();

    if after.allocation_count != before.allocation_count {
        return fail!(
            "synchronize_rcu allocated {} time(s)",
            after.allocation_count - before.allocation_count
        );
    }
    TestResult::Pass
}

/// A grace period elapses, and the caller observes it having elapsed.
///
/// The counterpart to the arithmetic tests in `slopos_ostd::sync::rcu`: those
/// say the target is computed correctly, this says a real machine reaches it.
pub fn test_synchronize_rcu_completes_a_grace_period() -> TestResult {
    let before = slopos_ostd::sync::rcu_gp_seq();
    slopos_ostd::sync::synchronize_rcu();
    let after = slopos_ostd::sync::rcu_gp_seq();

    if after.wrapping_sub(before) < 2 {
        return fail!(
            "grace-period sequence advanced {} (want >= 2)",
            after - before
        );
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_rcu_callbacks_are_invoked_without_a_manual_drain,
    suite = rcu_cb
);
slopos_testing::stest!(
    name = test_synchronize_rcu_allocates_nothing,
    suite = rcu_cb
);
slopos_testing::stest!(
    name = test_synchronize_rcu_completes_a_grace_period,
    suite = rcu_cb
);
