//! In-kernel lockdep self-tests.
//!
//! The `zz_` prefix sorts these last inside `slopos_testing`, itself the
//! lexicographically-last kernel test crate, so they run at the very end of the
//! kernel phase.

use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_graph;

use crate::{assert_test, TestResult};

/// Synthetic class identities, chosen outside every kernel mapping so a stray
/// deref faults loudly rather than corrupting a real lock.
const SELF_TEST_A: usize = 0x1000_0000_0000_1001;
const SELF_TEST_B: usize = 0x1000_0000_0000_2002;

/// `lockdep=off` is a supported mode, so checks below skip rather than fail:
/// they are claims about a running validator, not about the kernel.
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

        session.push(class_a);
        session.push(class_b);
        session.pop(class_b);
        session.pop(class_a);
        assert_test!(
            lock_graph::held_lock_count() == 0,
            "held stack not drained after chain 1"
        );

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

/// A validator that disabled itself must say so: the `GRAPH_OVERFLOW` warning is
/// the only trace in a boot log that later acquires went unvalidated.
fn lockdep_overflow_is_announced() -> TestResult {
    if lock_graph::graph_overflowed() {
        assert_test!(
            lock_graph::overflow_reported(),
            "GRAPH_OVERFLOW latched but no warning was ever emitted"
        );
    }
    TestResult::Pass
}

/// Class identity is the declaration site, so an array of N like locks costs one
/// class: an overflow here means a pool genuinely ran out.
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

/// The entry write and the depth publish are separate stores: an interrupt
/// between them leaves a null address counted inside `depth` that no `pop_lock`
/// can find, so that CPU reports a lock held forever.
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

/// Not `== 0`: a recovered panic drains the held stack, and every live guard's
/// `Drop` then pops an address that is gone, so the drained count is the bound.
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

/// Two declaration sites whose ids collide validate as one class, a false
/// positive nothing in the reported code explains; rename one of the sites.
fn lockdep_no_class_collisions() -> TestResult {
    assert_test!(
        lock_graph::class_collisions() == 0,
        "{} class-id collision(s) — distinct declaration sites are being \
         validated as one class",
        lock_graph::class_collisions(),
    );
    TestResult::Pass
}

/// Share of a pool the kernel phase must stay under. 70%, not 100%: a ceiling at
/// the pool size fires only once the validator has already turned itself off.
const MAX_POOL_FILL_PCT: usize = 70;

/// A floor, because "0 classes" satisfies every ceiling: without it a validator
/// that stopped registering reports as maximally healthy.
const MIN_CLASSES: usize = 32;

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
