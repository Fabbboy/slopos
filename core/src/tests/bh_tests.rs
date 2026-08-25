//! Kernel-side tests for the bottom-half point.
//!
//! Every context predicate is load-bearing: a drain that runs with a lock held
//! reaches the allocator underneath it, and one that re-enters itself recurses
//! against a 4 KiB stack. None of that is visible from the producer side, which
//! only ever sets a byte.

use slopos_ostd::lock_class;
use slopos_ostd::sync::bh;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_testing::TestResult;
use slopos_testing::fail;

static PROBE_LOCK: SpinLock<u32> = SpinLock::new(0, lock_class!("BH_TEST", LOCK_LEVEL_RESOURCE));

/// The deferred work frees memory, and the buddy's reuse path is exactly where
/// a caller's own lock would meet it.
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
/// restoring the caller's interrupt flag and releasing its preemption guard last.
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
        // `bh::drains()` is per-CPU and the two reads are not pinned to one, so this can wrap.
        return fail!(
            "only {} of {} outermost unlocks reached the drain",
            drains.wrapping_sub(before),
            UNLOCK_ROUNDS
        );
    }
    TestResult::Pass
}

/// The deferred work takes locks, so every drain contains releases that reach
/// this same hook. Without the claim each would start a nested drain from the
/// destructor of the guard the outer one is standing on, and a callback that
/// re-raised would make that recursion unbounded.
pub fn test_bh_is_not_re_entered() -> TestResult {
    // The claim and its restore are both gs-relative: a migration across this window would
    // strand the claiming CPU marked as draining, which the check below reports but cannot undo.
    let claimed_on = slopos_arch::pcr::get_current_cpu();
    let claimed_before = slopos_arch::pcr::bh_active_swap(true);

    let guard = PROBE_LOCK.lock();
    let drains_before = bh::drains();
    let reentrant_before = bh::declined_reentrant();
    bh::raise();
    drop(guard);
    let drains = bh::drains();
    let reentrant = bh::declined_reentrant();

    let restored_on = slopos_arch::pcr::get_current_cpu();
    slopos_arch::pcr::bh_active_swap(claimed_before);

    if restored_on != claimed_on {
        return fail!(
            "the test migrated from CPU {} to {} holding the bottom-half claim — CPU {} is \
             left marked as draining and no restore from here can reach it",
            claimed_on,
            restored_on,
            claimed_on
        );
    }
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
