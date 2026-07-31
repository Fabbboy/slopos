//! In-kernel lockdep self-tests.
//!
//! The module name sorts last (`zz_`) inside `slopos_testing`, which is itself
//! the lexicographically-last kernel test crate, so these run at the very end
//! of the kernel phase — the point at which "is the validator still alive?" is
//! a meaningful question.
//!
//! The gate pattern `check_task_ownership.sh --self-test` already uses applies
//! here: a validator that has never been observed to fire has not been observed
//! to work. `lock_graph.rs` is a real lockdep — dependency-edge learning, BFS
//! cycle detection, a chain-hash cache — and until these tests existed nothing
//! demonstrated that any of it ran in a booted kernel.

use slopos_ostd::sync::lock_graph;

use crate::{assert_test, TestResult};

/// Synthetic class identities.
///
/// `push_lock` treats the pointer purely as an identity token and the poison
/// callback below is a no-op, so nothing ever dereferences these. Chosen
/// outside every kernel mapping so a future stray deref faults loudly rather
/// than corrupting a real lock.
const SELF_TEST_A: usize = 0x1000_0000_0000_1001;
const SELF_TEST_B: usize = 0x1000_0000_0000_2002;

/// Poison callback for the synthetic classes. Reached only by
/// `poison_unlock_all_held` during a fatal abort, where there is no lock at
/// these addresses to unlock.
unsafe fn noop_poison(_addr: *const ()) {}

/// Take two synthetic locks in both orders and assert the cycle detector
/// reports the inversion.
///
/// Four preconditions have to hold for this to mean anything, and three of them
/// are false by the time the suite runs: `GRAPH_OVERFLOW` is latched during
/// memory init (256 `PROCESS_VMS` locks exhaust a 256-entry class table),
/// `PANIC_BYPASS` is latched by `bootstrap_ccc_panic_canary`'s deliberate
/// unwind, and the class table has no headroom left. `SelfTestGuard` overrides
/// the first two — overriding rather than clearing, so the boot-end assertion
/// below still reads reality — and the reserved slots supply the third.
///
/// The assertion is on `violations_reported()`, which is bumped inside
/// `report_cycle` after `path_exists` returned true. That is a claim about the
/// cycle detector specifically, not about "something panicked".
fn lockdep_ab_ba_is_detected() -> TestResult {
    assert_test!(
        lock_graph::tracking_enabled(),
        "lock tracking is not enabled — nothing below can fire"
    );

    let a = core::ptr::without_provenance::<()>(SELF_TEST_A);
    let b = core::ptr::without_provenance::<()>(SELF_TEST_B);

    let Some(_class_a) = lock_graph::reserve_self_test_class(0, a, lock_graph::LOCK_LEVEL_RESOURCE)
    else {
        return crate::fail!("could not reserve self-test class A");
    };
    let Some(_class_b) = lock_graph::reserve_self_test_class(1, b, lock_graph::LOCK_LEVEL_RESOURCE)
    else {
        return crate::fail!("could not reserve self-test class B");
    };

    let before = lock_graph::violations_reported();
    {
        let _session = lock_graph::SelfTestGuard::begin();

        // Chain 1: A then B. Learns the edge A -> B.
        // SAFETY: synthetic addresses that are never dereferenced, paired
        // LIFO, on a CPU running the test harness with no other guard live.
        unsafe {
            lock_graph::push_lock(a, noop_poison, lock_graph::LOCK_LEVEL_RESOURCE);
            lock_graph::push_lock(b, noop_poison, lock_graph::LOCK_LEVEL_RESOURCE);
            lock_graph::pop_lock(b);
            lock_graph::pop_lock(a);
        }
        assert_test!(
            lock_graph::held_lock_count() == 0,
            "held stack not drained after chain 1"
        );

        // Chain 2: B then A. Pushing A while B is held makes
        // `path_exists(A, B)` true via the edge learned above, which is what
        // `report_cycle` fires on. Report-only mode counts and logs instead of
        // panicking, so the harness fixture is left clean and `PANIC_BYPASS`
        // is not latched by this test.
        // SAFETY: as above.
        unsafe {
            lock_graph::push_lock(b, noop_poison, lock_graph::LOCK_LEVEL_RESOURCE);
            lock_graph::push_lock(a, noop_poison, lock_graph::LOCK_LEVEL_RESOURCE);
            lock_graph::pop_lock(a);
            lock_graph::pop_lock(b);
        }
    }
    let after = lock_graph::violations_reported();

    assert_test!(
        lock_graph::held_lock_count() == 0,
        "held stack not drained after chain 2"
    );
    // `>` rather than `== before + 1`: the override also re-enables validation
    // for every other CPU for the duration of the window, so another CPU may
    // legitimately report inside it.
    assert_test!(
        after > before,
        "validator did not report the A->B / B->A inversion"
    );
    TestResult::Pass
}

/// A validator that disables itself must say so.
///
/// The four `GRAPH_OVERFLOW` latch sites used to be silent, so the fact that
/// every lock acquired after memory init went unvalidated was invisible in
/// every boot log. Green today; fails if the warning is lost to a refactor.
fn lockdep_overflow_is_announced() -> TestResult {
    if lock_graph::graph_overflowed() {
        assert_test!(
            lock_graph::overflow_reported(),
            "GRAPH_OVERFLOW latched but no warning was ever emitted"
        );
    }
    TestResult::Pass
}

/// End-of-kernel-phase assertion that the validator was never disabled.
///
/// Reports `Skipped` while class identity is the lock *instance* address:
/// `init_process_vm` acquires 256 distinct `PROCESS_VMS` locks and the class
/// table holds 256, so the table is exhausted before the event bus, the TTY
/// table, the TCP shards or the futex buckets have acquired anything. Giving
/// `SpinLock::new` a declaration-site `&'static LockClassKey` collapses those
/// 256 classes into one; when that lands this flips to `Pass` with no edit
/// here.
fn lockdep_graph_overflow_clear_at_boot_end() -> TestResult {
    if lock_graph::graph_overflowed() {
        slopos_ostd::klog_warn!(
            "LOCKDEP SELF-TEST: GRAPH_OVERFLOW latched at end of kernel phase \
             (classes {}/{}); expected while lock classes are keyed on the lock \
             instance address",
            lock_graph::class_count(),
            lock_graph::REGISTRABLE_CLASSES,
        );
        return TestResult::Skipped;
    }
    TestResult::Pass
}

crate::stest!(name = lockdep_ab_ba_is_detected);
crate::stest!(name = lockdep_graph_overflow_clear_at_boot_end);
crate::stest!(name = lockdep_overflow_is_announced);
