//! Kernel-side tests for the bottom-half point.
//!
//! Every context predicate is load-bearing: a drain that runs with a lock held
//! reaches the allocator underneath it, and one that re-enters itself recurses
//! against a 4 KiB stack. None of that is visible from the producer side, which
//! only ever sets a byte.
//!
//! The counters are per-CPU and `SpinLock::lock` masks this CPU's interrupts,
//! so a window opened inside the critical section admits neither a peer CPU's
//! drain nor this CPU's own timer tick. That is what lets the deltas below be
//! exact rather than bounds, and it is why each snapshot is taken after the
//! lock rather than before it.

use slopos_ostd::lock_class;
use slopos_ostd::sync::bh;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_testing::TestResult;
use slopos_testing::fail;

static PROBE_LOCK: SpinLock<u32> = SpinLock::new(0, lock_class!("BH_TEST", LOCK_LEVEL_RESOURCE));

/// The deferred work frees memory, and the buddy's reuse path is exactly where
/// a caller's own lock would meet it. What is pinned is that the call declines
/// instead of draining, not which predicate refused it: `SpinLock` masks
/// interrupts too, so no caller can hold a tracked lock and leave the
/// interrupt-enable test the one that still passes.
pub fn test_bh_declines_under_a_held_lock() -> TestResult {
    let guard = PROBE_LOCK.lock();
    let declines_before = bh::declined_context();
    let drains_before = bh::drains();
    bh::raise();
    bh::run_pending_if_due();
    let declines = bh::declined_context();
    let drains = bh::drains();
    drop(guard);

    if drains != drains_before {
        return fail!("a drain ran with a lock held");
    }
    if declines != declines_before + 1 {
        return fail!(
            "expected exactly one decline with a lock held, saw {}",
            declines - declines_before
        );
    }
    TestResult::Pass
}

/// Running the work in an interrupts-off window would hold off the timer tick —
/// including the one that advances the grace period the work is waiting on.
pub fn test_bh_declines_with_interrupts_off() -> TestResult {
    let (drained, declines) = slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        let declines_before = bh::declined_context();
        let drains_before = bh::drains();
        bh::raise();
        bh::run_pending_if_due();
        (
            bh::drains() != drains_before,
            bh::declined_context() - declines_before,
        )
    });

    if drained {
        return fail!("a drain ran with interrupts disabled");
    }
    if declines != 1 {
        return fail!(
            "expected exactly one decline with interrupts disabled, saw {}",
            declines
        );
    }
    TestResult::Pass
}

/// `raise` is called from hard-IRQ handlers and from under cli-spinlocks, so it
/// has to stay a single store; a version that drained inline would deadlock at
/// the first such caller.
pub fn test_bh_raise_does_not_run_work_inline() -> TestResult {
    let guard = PROBE_LOCK.lock();
    let drains_before = bh::drains();
    let declines_before = bh::declined_context();
    bh::raise();
    let drains = bh::drains();
    let declines = bh::declined_context();
    drop(guard);

    if drains != drains_before || declines != declines_before {
        return fail!("raise() reached the drain instead of only setting a flag");
    }
    TestResult::Pass
}

/// The only drain point a lock-taking kernel thread that never returns to
/// userland reaches. Firing on the guard's own drop rests on `SpinLockGuard`
/// restoring the caller's interrupt flag and then releasing its preemption
/// guard last.
///
/// The count can only be read once that flag is back on, so a tick landing in
/// the handful of instructions before the read could drain and stand in for an
/// unlock that failed to. Repeating the sequence demands that coincidence on
/// every round instead of once.
pub fn test_bh_runs_at_the_outermost_unlock() -> TestResult {
    const UNLOCK_ROUNDS: u64 = 8;

    let mut before = 0;
    for round in 0..UNLOCK_ROUNDS {
        let guard = PROBE_LOCK.lock();
        if round == 0 {
            before = bh::drains();
        }
        bh::raise();
        drop(guard);
    }

    let drains = bh::drains();
    if drains < before + UNLOCK_ROUNDS {
        return fail!(
            "only {} of {} outermost unlocks reached the drain",
            drains - before,
            UNLOCK_ROUNDS
        );
    }
    TestResult::Pass
}

/// The deferred work takes locks, so every drain contains releases that reach
/// this same hook. Without the claim each would start a nested drain from the
/// destructor of the guard the outer one is standing on, and a callback that
/// re-raised would make that recursion unbounded.
///
/// The claim is taken by hand because no drain callback is reachable from a
/// test: inside the drain's own preemption guard a nested release never fires
/// the hook at all, so the claim is only ever consulted from the relaxed phase,
/// which runs in exactly the state reconstructed here — preemption on,
/// interrupts on, no lock held. Holding it across the same unlock
/// [`test_bh_runs_at_the_outermost_unlock`] shows does drain leaves the claim as
/// the only difference between the two.
pub fn test_bh_is_not_re_entered() -> TestResult {
    let claimed_before = slopos_arch::pcr::bh_active_swap(true);

    let guard = PROBE_LOCK.lock();
    let drains_before = bh::drains();
    let reentrant_before = bh::declined_reentrant();
    bh::raise();
    drop(guard);
    let drains = bh::drains();
    let reentrant = bh::declined_reentrant();

    slopos_arch::pcr::bh_active_swap(claimed_before);

    if claimed_before {
        return fail!("a drain was already claimed on this CPU outside one");
    }
    if drains != drains_before {
        return fail!("a drain nested inside one this CPU had already claimed");
    }
    if reentrant == reentrant_before {
        return fail!("the nested drain was neither declined as re-entrant nor run");
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_bh_declines_under_a_held_lock, suite = bh);
slopos_testing::stest!(name = test_bh_declines_with_interrupts_off, suite = bh);
slopos_testing::stest!(name = test_bh_raise_does_not_run_work_inline, suite = bh);
slopos_testing::stest!(name = test_bh_runs_at_the_outermost_unlock, suite = bh);
slopos_testing::stest!(name = test_bh_is_not_re_entered, suite = bh);
