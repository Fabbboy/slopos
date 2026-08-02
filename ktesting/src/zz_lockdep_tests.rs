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

use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_graph;

use crate::{assert_test, TestResult};

/// Synthetic class identities.
///
/// `push_lock` treats the pointer purely as an identity token and the poison
/// callback is a no-op, so nothing ever dereferences these. Chosen outside
/// every kernel mapping so a future stray deref faults loudly rather than
/// corrupting a real lock.
const SELF_TEST_A: usize = 0x1000_0000_0000_1001;
const SELF_TEST_B: usize = 0x1000_0000_0000_2002;

/// Take two synthetic locks in both orders and assert the cycle detector
/// reports the inversion.
///
/// The assertion is on `report_only_violations()`, which `report_cycle` bumps
/// after `path_exists` returned true. That is a claim about the cycle detector
/// specifically, not about "something panicked". The guard keeps the reporter
/// in report-only mode so provoking a cycle does not take the machine down;
/// the reserved class slots keep the test working whatever the class count is.
/// `lockdep=off` is a supported mode — it is how the validator's own
/// per-acquire cost is measured — and the checks below are claims about a
/// running validator, not about the kernel. Skipping says "not asked", which
/// is the truth; failing would make the measurement run report a broken
/// kernel.
fn validator_deliberately_off() -> bool {
    lock_graph::lockdep_mode() == lock_graph::LockdepMode::Off
}

fn lockdep_ab_ba_is_detected() -> TestResult {
    if validator_deliberately_off() {
        return TestResult::Skipped;
    }
    assert_test!(
        lock_graph::validator_alive(),
        "validator is not alive (tracking={} overflow={} bypass={} mode={:?}) — \
         the cycle detector cannot fire, so nothing below proves anything",
        lock_graph::tracking_enabled(),
        lock_graph::graph_overflowed(),
        lock_graph::fatal_bypassed(),
        lock_graph::lockdep_mode(),
    );

    let a = core::ptr::without_provenance::<()>(SELF_TEST_A);
    let b = core::ptr::without_provenance::<()>(SELF_TEST_B);

    let key_a = lock_class!("selftest.A", lock_graph::LOCK_LEVEL_RESOURCE);
    let key_b = lock_class!("selftest.B", lock_graph::LOCK_LEVEL_RESOURCE);

    let Some(class_a) = lock_graph::reserve_self_test_class(0, key_a, a) else {
        return crate::fail!("could not reserve self-test class A");
    };
    let Some(class_b) = lock_graph::reserve_self_test_class(1, key_b, b) else {
        return crate::fail!("could not reserve self-test class B");
    };

    let before = lock_graph::report_only_violations();
    {
        let session = lock_graph::SelfTestGuard::begin();

        // Chain 1: A then B. Learns the edge A -> B.
        session.push(class_a);
        session.push(class_b);
        session.pop(class_b);
        session.pop(class_a);
        assert_test!(
            lock_graph::held_lock_count() == 0,
            "held stack not drained after chain 1"
        );

        // Chain 2: B then A. Pushing A while B is held makes
        // `path_exists(A, B)` true via the edge learned above, which is what
        // `report_cycle` fires on.
        session.push(class_b);
        session.push(class_a);
        session.pop(class_a);
        session.pop(class_b);
    }
    let after = lock_graph::report_only_violations();

    assert_test!(
        lock_graph::held_lock_count() == 0,
        "held stack not drained after chain 2"
    );
    // `>` rather than `== before + 1`: report-only mode is global, so another
    // CPU may legitimately report inside the window.
    assert_test!(
        after > before,
        "validator did not report the A->B / B->A inversion"
    );
    TestResult::Pass
}

/// A validator that disables itself must say so.
///
/// The `GRAPH_OVERFLOW` latch sites are the only thing that makes "every lock
/// acquired from here on is unvalidated" visible in a boot log. Green when
/// nothing overflowed; fails if the warning is lost to a refactor.
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
/// Class identity is the declaration site, so an array of N like locks costs
/// one class rather than N and the table is not exhaustible by a loop over a
/// lock array. Overflow here means a real pool ran out.
fn lockdep_graph_overflow_clear_at_boot_end() -> TestResult {
    assert_test!(
        !lock_graph::graph_overflowed(),
        "GRAPH_OVERFLOW latched (classes {}/{} edges {}/{} chains {}/{}) — every \
         lock acquired after the latch was UNVALIDATED",
        lock_graph::class_count(),
        lock_graph::REGISTRABLE_CLASSES,
        lock_graph::edge_count(),
        lock_graph::MAX_EDGES,
        lock_graph::chain_count(),
        lock_graph::MAX_CHAINS,
    );
    TestResult::Pass
}

/// Every held-stack update this boot ran with interrupts masked.
///
/// The entry write and the depth publish are separate stores, so an interrupt
/// between them lets the handler's own acquire claim the slot the interrupted
/// push had filled but not yet counted. What it leaves behind is an entry with
/// a null address counted inside `depth`, which no `pop_lock` can find — from
/// then on that CPU reports a lock held forever, and every consumer of the
/// held stack reads a corrupt one.
///
/// A whole boot is the test: `PreemptMutex` and `Epoch::enter` acquire with
/// interrupts on, the epoch one on every received TCP segment, so the acquire
/// this covers has run tens of thousands of times by the time it is asked.
fn lockdep_updates_run_with_interrupts_masked() -> TestResult {
    assert_test!(
        lock_graph::push_irq_state() == lock_graph::PushIrqState::ReachedMasked,
        "held-stack updates were {:?}, want ReachedMasked — an interrupt landing \
         inside one leaves an entry that cannot be popped, and NotReached would \
         mean the validator never ran at all",
        lock_graph::push_irq_state(),
    );
    TestResult::Pass
}

/// Two declaration sites whose ids collide are validated as one class, which
/// can produce a false positive that no amount of reading the reported code
/// would explain. The hash is 64-bit over `file:line:column`, so this should
/// never fire; if it does, one of the two sites needs renaming.
/// No release went unmatched beyond what the poison walk drained.
///
/// A release whose address is not on the stack is the instruction that makes
/// a depth leak permanent — `pop_lock` returns without decrementing and that
/// CPU reports a lock held forever. Not `== 0`: a recovered panic drains the
/// stack, and every live guard's `Drop` then pops an address that is gone, so
/// the honest bound is the drained count.
fn lockdep_no_unmatched_releases() -> TestResult {
    let misses = lock_graph::pop_misses();
    let drained = lock_graph::poison_drained();
    assert_test!(
        misses <= drained,
        "{} release(s) found no entry against {} drained by the poison walk —          the excess is depth that will never be given back",
        misses,
        drained,
    );
    TestResult::Pass
}

fn lockdep_no_class_collisions() -> TestResult {
    assert_test!(
        lock_graph::class_collisions() == 0,
        "{} class-id collision(s) — distinct declaration sites are being \
         validated as one class",
        lock_graph::class_collisions(),
    );
    TestResult::Pass
}

/// Share of a pool the kernel phase must stay under.
///
/// 70%, not 100%: the failure this catches is a new array-of-locks static
/// landing without a class key, which adds classes by the hundred. A ceiling
/// at the pool size only fires once the validator has already turned itself
/// off, which is the state this whole mechanism exists to avoid.
const MAX_POOL_FILL_PCT: usize = 70;

/// Classes the kernel phase must have registered for the numbers above it to
/// mean anything. Without a floor, "0 classes" satisfies every ceiling and a
/// validator that stopped registering reports as maximally healthy — the same
/// hole `min-records` plugs in the stack-size gate.
const MIN_CLASSES: usize = 32;

/// Pool headroom at the end of the kernel phase, logged and bounded.
fn lockdep_pool_headroom() -> TestResult {
    if validator_deliberately_off() {
        return TestResult::Skipped;
    }
    let (c, e, ch) = (
        lock_graph::class_count(),
        lock_graph::edge_count(),
        lock_graph::chain_count(),
    );
    let (cc, ec, cch) = (
        lock_graph::REGISTRABLE_CLASSES,
        lock_graph::MAX_EDGES,
        lock_graph::MAX_CHAINS,
    );
    slopos_ostd::klog_info!(
        "LOCKDEP HEADROOM: classes={}/{} ({}%) edges={}/{} ({}%) chains={}/{} ({}%) \
         held_max={}/{} held_drops={} pop_miss={}/{} chain_hit={} chain_miss={} \
         slots_leaked={}",
        c,
        cc,
        c * 100 / cc,
        e,
        ec,
        e * 100 / ec,
        ch,
        cch,
        ch * 100 / cch,
        lock_graph::held_depth_max(),
        lock_graph::MAX_HELD_LOCKS,
        lock_graph::held_depth_overflows(),
        lock_graph::pop_misses(),
        lock_graph::poison_drained(),
        lock_graph::chain_hits(),
        lock_graph::chain_misses(),
        lock_graph::class_slots_leaked(),
    );

    assert_test!(
        c >= MIN_CLASSES,
        "only {} classes registered — the validator is not measuring anything",
        c
    );
    assert_test!(
        c * 100 / cc <= MAX_POOL_FILL_PCT,
        "class pool {}% full ({}/{})",
        c * 100 / cc,
        c,
        cc
    );
    assert_test!(
        e * 100 / ec <= MAX_POOL_FILL_PCT,
        "edge pool {}% full ({}/{})",
        e * 100 / ec,
        e,
        ec
    );
    assert_test!(
        ch * 100 / cch <= MAX_POOL_FILL_PCT,
        "chain pool {}% full ({}/{})",
        ch * 100 / cch,
        ch,
        cch
    );
    assert_test!(
        lock_graph::held_depth_overflows() == 0,
        "{} push(es) exceeded MAX_HELD_LOCKS={} and were DROPPED — those locks are \
         invisible to the poison walk and cannot be found by pop_lock",
        lock_graph::held_depth_overflows(),
        lock_graph::MAX_HELD_LOCKS,
    );
    TestResult::Pass
}

/// A violation counted in a kernel that is still running is a real finding
/// nobody saw: the reporter fired and something swallowed the panic.
fn lockdep_no_unreported_violations() -> TestResult {
    assert_test!(
        lock_graph::violations_reported() == 0,
        "{} ordering violation(s) reported with the panic suppressed",
        lock_graph::violations_reported()
    );
    TestResult::Pass
}

crate::stest!(name = lockdep_ab_ba_is_detected);
crate::stest!(name = lockdep_no_unreported_violations);
crate::stest!(name = lockdep_pool_headroom);
crate::stest!(name = lockdep_graph_overflow_clear_at_boot_end);
crate::stest!(name = lockdep_no_class_collisions);
crate::stest!(name = lockdep_updates_run_with_interrupts_masked);
crate::stest!(name = lockdep_no_unmatched_releases);
crate::stest!(name = lockdep_overflow_is_announced);
