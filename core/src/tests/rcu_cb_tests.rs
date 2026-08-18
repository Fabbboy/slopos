//! Kernel-side tests for RCU deferred-callback reclamation.
//!
//! `call_rcu` only queues, so these tests never call the drain by hand: a queue
//! whose consumer is unreachable looks exactly like a working one from the
//! producer's side.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_kernel_services::platform::{clock_monotonic_ns, timer_poll_delay_ms};
use slopos_ostd::KArc;
use slopos_ostd::sync::RcuArcSlot;
use slopos_testing::TestResult;
use slopos_testing::fail;

/// How long to give the drain before calling it unreachable: it runs when a CPU
/// finds nothing to dispatch, so the bound is a scheduling property.
const RECLAIM_DEADLINE_MS: u32 = 400;

/// Polled rather than slept: the kernel test phase runs on the BSP before it
/// enters the scheduler, so this task cannot block.
const POLL_INTERVAL_MS: u32 = 10;

/// Drive the RCU drain until `done` holds, or the deadline expires. Polls for
/// the effect rather than trusting the manual call: a CPU that already detached
/// the chain leaves nothing for that call to find while the callback is in
/// flight.
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

/// `RcuArcSlot::store` defers the displaced reference through `call_rcu`, so
/// the payload's `Drop` runs only if the callback does.
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

/// Called from the reclaim path, including `call_rcu`'s own out-of-memory
/// fallback, so an allocation here fails exactly when there is nothing to
/// allocate from.
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

/// A drain pass never takes a grace period inline. Putting the wait back would
/// still pass every correctness test and show up only as latency, so the passes
/// are timed.
pub fn test_rcu_drain_never_waits_for_a_grace_period() -> TestResult {
    const PASSES: u32 = 32;
    // 32 passes each taking a grace period would be seconds, not milliseconds.
    const BUDGET_MS: u64 = 50;

    // Queue work so the passes have something to tag and retire rather than
    // early-returning on an empty backlog.
    for _ in 0..8 {
        let slot = RcuArcSlot::<DropCounted>::empty();
        let Ok(value) = KArc::try_new(DropCounted) else {
            return fail!("KArc allocation failed");
        };
        slot.store(Some(value));
        slot.store(None);
    }

    let start = clock_monotonic_ns();
    for _ in 0..PASSES {
        slopos_ostd::sync::rcu_raise_softirq();
        slopos_ostd::sync::rcu_process_callbacks();
    }
    let elapsed_ms = clock_monotonic_ns().saturating_sub(start) / 1_000_000;

    if elapsed_ms > BUDGET_MS {
        return fail!(
            "{} drain passes took {}ms (budget {}ms) — the invoke step is waiting again",
            PASSES,
            elapsed_ms,
            BUDGET_MS
        );
    }
    TestResult::Pass
}

/// `rcu_barrier` waits for invocation, not merely for a grace period — once
/// the drain is asynchronous those stop being the same fact, and a caller
/// tearing down what a callback will touch needs the first.
pub fn test_rcu_barrier_waits_for_invocation() -> TestResult {
    let before = DROPPED.load(Ordering::Acquire);

    let slot = RcuArcSlot::<DropCounted>::empty();
    let Ok(value) = KArc::try_new(DropCounted) else {
        return fail!("KArc allocation failed");
    };
    slot.store(Some(value));
    slot.store(None);

    slopos_ostd::sync::rcu_barrier();

    if DROPPED.load(Ordering::Acquire) == before {
        return fail!("rcu_barrier returned with a callback still pending");
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_rcu_callbacks_are_invoked_without_a_manual_drain,
    suite = rcu_cb
);
slopos_testing::stest!(
    name = test_rcu_drain_never_waits_for_a_grace_period,
    suite = rcu_cb
);
slopos_testing::stest!(name = test_rcu_barrier_waits_for_invocation, suite = rcu_cb);
slopos_testing::stest!(
    name = test_synchronize_rcu_allocates_nothing,
    suite = rcu_cb
);
slopos_testing::stest!(
    name = test_synchronize_rcu_completes_a_grace_period,
    suite = rcu_cb
);
