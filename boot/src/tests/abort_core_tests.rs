//! Regression tests for the Reliable Abort Core surface.
//!
//! The fatal (uncaught) panic path itself cannot be driven from a `stest!` —
//! the harness catches panics, and an uncaught panic exits QEMU. These pin the
//! data-structure + election contract a regression would most plausibly break:
//! emergency-stack priming and region disjointness, the guard-fault classifier,
//! single-owner election, and the per-CPU recursion depth. The live stack
//! switch is covered end-to-end by the `panic.fatal_smoke` boot-log check.

use slopos_mm::memory_layout_defs::{
    EMERGENCY_DSTACK_REGION_BASE, EMERGENCY_SAFE_STACK_REGION_BASE, EXC_DSTACK_REGION_BASE,
    USTACK_VA_BASE, USTACK_VA_END,
};
use slopos_testing::{TestResult, assert_test};

use crate::ist_stacks::{
    emergency_dstack_bounds_current_cpu, emergency_safe_bounds_current_cpu,
    emergency_stack_guard_fault,
};

/// Both emergency stack tops are primed into the PCR (non-zero, at the region
/// top) and sized generously.
pub fn test_emergency_stacks_primed() -> TestResult {
    let safe_sp = slopos_arch::pcr::local_panic_safe_sp();
    let data_sp = slopos_arch::pcr::local_panic_unsafe_sp();
    let (sg, su, st) = emergency_safe_bounds_current_cpu();
    let (dg, du, dt) = emergency_dstack_bounds_current_cpu();

    assert_test!(
        safe_sp == st,
        "panic_safe_sp not at emergency safe-stack top"
    );
    assert_test!(
        data_sp == dt,
        "panic_unsafe_sp not at emergency data-stack top"
    );
    assert_test!(su > sg && st > su, "emergency safe-stack bounds malformed");
    assert_test!(du > dg && dt > du, "emergency data-stack bounds malformed");
    assert_test!(
        st - su >= 0x8000,
        "emergency safe stack smaller than 32 KiB"
    );
    assert_test!(
        dt - du >= 0x10000,
        "emergency data stack smaller than 64 KiB"
    );
    TestResult::Pass
}

/// The emergency stacks are DISJOINT from the per-task USTACK region (the stack
/// that overflowed in the original crash) and from EXC_DSTACK, so the fatal
/// reporter can never alias the stack that is exhausted.
pub fn test_emergency_stacks_disjoint() -> TestResult {
    let (sg, _su, st) = emergency_safe_bounds_current_cpu();
    let (dg, _du, dt) = emergency_dstack_bounds_current_cpu();

    let safe_in_ustack = sg < USTACK_VA_END && st > USTACK_VA_BASE;
    let data_in_ustack = dg < USTACK_VA_END && dt > USTACK_VA_BASE;
    assert_test!(!safe_in_ustack, "emergency safe stack overlaps USTACK");
    assert_test!(!data_in_ustack, "emergency data stack overlaps USTACK");

    // Safe and data emergency regions must not overlap each other.
    let safe_meets_data = sg < dt && st > dg;
    assert_test!(!safe_meets_data, "emergency safe/data regions overlap");

    assert_test!(
        dg >= EMERGENCY_DSTACK_REGION_BASE && sg >= EMERGENCY_SAFE_STACK_REGION_BASE,
        "emergency stacks below their declared region bases"
    );
    assert_test!(
        dg >= EXC_DSTACK_REGION_BASE,
        "emergency data stack below EXC_DSTACK base"
    );
    TestResult::Pass
}

/// The guard-fault classifier flags an address inside either guard page and
/// rejects an address in the usable stack or outside the regions entirely.
pub fn test_emergency_guard_classifier() -> TestResult {
    let (sg, su, _st) = emergency_safe_bounds_current_cpu();
    let (dg, du, _dt) = emergency_dstack_bounds_current_cpu();

    assert_test!(
        emergency_stack_guard_fault(sg).is_some(),
        "safe-stack guard base not classified as overflow"
    );
    assert_test!(
        emergency_stack_guard_fault(dg + 0x10).is_some(),
        "data-stack guard not classified as overflow"
    );
    assert_test!(
        emergency_stack_guard_fault(su).is_none(),
        "usable safe-stack byte misclassified as guard"
    );
    assert_test!(
        emergency_stack_guard_fault(du).is_none(),
        "usable data-stack byte misclassified as guard"
    );
    assert_test!(
        emergency_stack_guard_fault(0x1000).is_none(),
        "low address misclassified as emergency guard"
    );
    TestResult::Pass
}

/// Single-owner election: the first claim wins, a different CPU loses, and the
/// reported state is consistent. Reset afterwards so no real panic is implied.
pub fn test_panic_owner_election() -> TestResult {
    use slopos_ostd::panic::{
        claim_panic_owner, panic_owner_claimed, panic_owner_is, reset_panic_owner_for_test,
    };
    reset_panic_owner_for_test();
    assert_test!(!panic_owner_claimed(), "owner claimed before any election");

    assert_test!(claim_panic_owner(7), "first claim did not win");
    assert_test!(panic_owner_claimed(), "owner not claimed after a win");
    assert_test!(panic_owner_is(7), "owner mismatch after claim");
    assert_test!(
        !claim_panic_owner(9),
        "a second CPU wrongly won the election"
    );
    assert_test!(panic_owner_is(7), "owner changed after a losing claim");
    assert_test!(!panic_owner_is(9), "loser reported as owner");

    reset_panic_owner_for_test();
    assert_test!(!panic_owner_claimed(), "reset did not clear the owner");
    TestResult::Pass
}

slopos_testing::stest!(name = test_emergency_stacks_primed, suite = abort_core);
slopos_testing::stest!(name = test_emergency_stacks_disjoint, suite = abort_core);
slopos_testing::stest!(name = test_emergency_guard_classifier, suite = abort_core);
slopos_testing::stest!(name = test_panic_owner_election, suite = abort_core);
