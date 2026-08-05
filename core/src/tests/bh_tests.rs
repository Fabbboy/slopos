//! Kernel-side tests for the bottom-half point.
//!
//! The point is a set of context predicates plus a re-entrancy claim, and every
//! one is load-bearing: a drain that runs with a lock held reaches the allocator
//! underneath it, and one that re-enters itself recurses on the hottest edge in
//! the kernel against a 4 KiB stack. None of that is visible from the producer
//! side, which only ever sets a byte.

use slopos_ostd::lock_class;
use slopos_ostd::sync::bh;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_testing::TestResult;
use slopos_testing::fail;

static PROBE_LOCK: SpinLock<u32> = SpinLock::new(0, lock_class!("BH_TEST", LOCK_LEVEL_RESOURCE));

/// A drain declines while a lock is held.
///
/// This is the predicate that keeps the point off the allocator's back: the
/// deferred work frees memory, and the buddy's reuse path is exactly where a
/// caller's own lock would meet it.
pub fn test_bh_declines_under_a_held_lock() -> TestResult {
    let declines_before = bh::declined_context();
    let drains_before = bh::drains();

    let guard = PROBE_LOCK.lock();
    bh::raise();
    bh::run_pending_if_due();
    let declines = bh::declined_context();
    let drains = bh::drains();
    drop(guard);

    if drains != drains_before {
        return fail!("a drain ran with a lock held");
    }
    if declines == declines_before {
        return fail!("the drain neither ran nor declined");
    }
    TestResult::Pass
}

/// A drain declines with interrupts off.
///
/// The work is long enough that running it in an interrupts-off window would
/// hold off the timer tick — including the one that advances the grace period
/// the work is waiting on.
pub fn test_bh_declines_with_interrupts_off() -> TestResult {
    let drains_before = bh::drains();
    let declines_before = bh::declined_context();

    let (declines, drains) = slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        bh::raise();
        bh::run_pending_if_due();
        (bh::declined_context(), bh::drains())
    });

    if drains != drains_before {
        return fail!("a drain ran with interrupts disabled");
    }
    if declines == declines_before {
        return fail!("the drain neither ran nor declined");
    }
    TestResult::Pass
}

/// Raising is legal from anywhere and never runs anything by itself.
///
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

/// The outermost unlock is a drain point.
///
/// This is what lets a lock-taking kernel thread that never returns to userland
/// make progress: it is the only site such a task reaches. The drain must fire
/// on the guard's own drop, which rests on `SpinLockGuard` releasing its
/// preemption guard last.
pub fn test_bh_runs_at_the_outermost_unlock() -> TestResult {
    let before = bh::drains();

    let guard = PROBE_LOCK.lock();
    bh::raise();
    drop(guard);

    if bh::drains() == before {
        return fail!("the outermost unlock did not reach the drain");
    }
    TestResult::Pass
}

/// A drain is not re-entered from a lock released inside it.
///
/// The deferred work takes locks, so every drain contains releases that reach
/// this same hook. Without the claim each would start a nested drain from the
/// destructor of the guard the outer one is standing on, and a callback that
/// re-raised would make that recursion unbounded.
pub fn test_bh_is_not_re_entered() -> TestResult {
    let reentrant_before = bh::declined_reentrant();

    // The drain's own work releases locks — the allocator's among them — so a
    // drain that ran at all should have refused at least one nested attempt.
    let guard = PROBE_LOCK.lock();
    bh::raise();
    drop(guard);

    if bh::declined_reentrant() < reentrant_before {
        return fail!("the re-entrancy counter went backwards");
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_bh_declines_under_a_held_lock, suite = bh);
slopos_testing::stest!(name = test_bh_declines_with_interrupts_off, suite = bh);
slopos_testing::stest!(name = test_bh_raise_does_not_run_work_inline, suite = bh);
slopos_testing::stest!(name = test_bh_runs_at_the_outermost_unlock, suite = bh);
slopos_testing::stest!(name = test_bh_is_not_re_entered, suite = bh);
