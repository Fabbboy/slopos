//! Kernel-side tests for RCU deferred-callback reclamation.
//!
//! `call_rcu` only queues, so the reachability test never calls the drain by
//! hand: a queue whose consumer is unreachable looks exactly like a working one
//! from the producer's side.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_kernel_services::platform::timer_poll_delay_ms;
use slopos_ostd::KArc;
use slopos_ostd::sync::RcuArcSlot;
use slopos_sched::test_fixture::KernelTestScope;
use slopos_testing::TestResult;
use slopos_testing::fail;

/// Quiescent-state reports to allow before calling the drain unreachable.
///
/// The budget is guest progress, not wall clock: this CPU's timer tick both
/// reports a quiescent state and raises the RCU softirq, so a descheduled vCPU
/// delivers no tick and spends none of the budget.
const RECLAIM_DEADLINE_REPORTS: u64 = 64;

/// Backstop on the budget above. Reaching it means this CPU stopped reporting
/// altogether, which is a dead timer tick rather than a slow host.
const TICK_WATCHDOG_MS: u32 = 5_000;

/// Polled rather than slept: the kernel test phase runs on the BSP before it
/// enters the scheduler, so this task cannot block.
const POLL_INTERVAL_MS: u32 = 10;

/// Drive every callback queued before this call to invocation, then report
/// `done`.
///
/// `rcu_barrier` waits for invocation rather than merely for a grace period and
/// cannot return early, so a `false` here is the callback failing to do its
/// work — not a deadline the host outran.
pub fn drain_until(done: impl Fn() -> bool) -> bool {
    slopos_ostd::sync::rcu_barrier();
    done()
}

static DROPPED: AtomicU32 = AtomicU32::new(0);

struct DropCounted;

impl Drop for DropCounted {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::Release);
    }
}

/// `RcuArcSlot::store` defers the displaced reference through `call_rcu`, so
/// the payload's `Drop` runs only if the callback does — and only if something
/// other than this test reaches the drain.
pub fn test_rcu_callbacks_are_invoked_without_a_manual_drain() -> TestResult {
    let cpu = slopos_arch::pcr::get_current_cpu();
    let before = DROPPED.load(Ordering::Acquire);
    let reports_before = slopos_ostd::sync::rcu_qs_counter(cpu);

    let slot = RcuArcSlot::<DropCounted>::empty();
    let Ok(value) = KArc::try_new(DropCounted) else {
        return fail!("KArc allocation failed");
    };
    slot.store(Some(value));
    // Displaces the only reference, which is what reaches `call_rcu`.
    slot.store(None);

    let mut waited = 0;
    loop {
        if DROPPED.load(Ordering::Acquire) != before {
            return TestResult::Pass;
        }

        let reports = slopos_ostd::sync::rcu_qs_counter(cpu).wrapping_sub(reports_before);
        if reports >= RECLAIM_DEADLINE_REPORTS {
            return fail!(
                "no RCU callback ran across {} quiescent-state reports on CPU {} — every \
                 deferred free since boot is leaking",
                reports,
                cpu
            );
        }
        if waited >= TICK_WATCHDOG_MS {
            return fail!(
                "CPU {} reported {} quiescent states in {}ms — its timer tick has stopped, so \
                 nothing drives the drain",
                cpu,
                reports,
                TICK_WATCHDOG_MS
            );
        }

        timer_poll_delay_ms(POLL_INTERVAL_MS);
        waited += POLL_INTERVAL_MS;
    }
}

/// Called from the reclaim path, including `call_rcu`'s own out-of-memory
/// fallback, so an allocation here fails exactly when there is nothing to
/// allocate from.
///
/// The heap counter is kernel-wide, so the scope is what makes it name this
/// caller: with the APs parked and the kernel-I/O threads frozen, no other
/// allocator is running to be mistaken for `synchronize_rcu`.
pub fn test_synchronize_rcu_allocates_nothing() -> TestResult {
    let _scope = KernelTestScope::enter();

    let before = slopos_mm::slab::get_heap_stats_owned();
    slopos_ostd::sync::synchronize_rcu();
    let after = slopos_mm::slab::get_heap_stats_owned();

    if after.allocation_count != before.allocation_count {
        return fail!(
            "synchronize_rcu allocated {} time(s)",
            after.allocation_count.wrapping_sub(before.allocation_count)
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
            after.wrapping_sub(before)
        );
    }
    TestResult::Pass
}

/// A drain pass never takes a grace period inline. Putting the wait back would
/// still pass every correctness test, so the passes are counted at the wait
/// itself: this CPU's entries into `synchronize_rcu`, which only the drain can
/// have made here, must not move.
pub fn test_rcu_drain_never_waits_for_a_grace_period() -> TestResult {
    const PASSES: u32 = 32;

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

    let cpu = slopos_arch::pcr::get_current_cpu();
    let before = slopos_ostd::sync::rcu_sync_entry_count(cpu);
    for _ in 0..PASSES {
        slopos_ostd::sync::rcu_raise_softirq();
        slopos_ostd::sync::rcu_process_callbacks();
    }
    let after = slopos_ostd::sync::rcu_sync_entry_count(cpu);

    if after != before {
        return fail!(
            "{} drain passes entered the grace-period wait {} time(s) — the invoke step is \
             waiting again",
            PASSES,
            after.wrapping_sub(before)
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
