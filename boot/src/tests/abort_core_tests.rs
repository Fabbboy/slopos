//! The Reliable Abort Core surface. A fatal panic cannot be driven from a
//! `stest!` (the harness catches panics), so these pin the data-structure and
//! election contract; `panic.fatal_smoke` covers the live stack switch.

use slopos_mm::memory_layout_defs::{
    EMERGENCY_DSTACK_REGION_BASE, EMERGENCY_SAFE_STACK_REGION_BASE, EXC_DSTACK_REGION_BASE,
    USTACK_VA_BASE, USTACK_VA_END,
};
use slopos_testing::{TestResult, assert_test};

use crate::ist_stacks::{
    emergency_dstack_bounds_current_cpu, emergency_safe_bounds_current_cpu,
    emergency_stack_guard_fault,
};

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

/// Disjoint from the per-task USTACK region and from EXC_DSTACK, so the fatal
/// reporter can never alias the stack that is exhausted.
pub fn test_emergency_stacks_disjoint() -> TestResult {
    let (sg, _su, st) = emergency_safe_bounds_current_cpu();
    let (dg, _du, dt) = emergency_dstack_bounds_current_cpu();

    let safe_in_ustack = sg < USTACK_VA_END && st > USTACK_VA_BASE;
    let data_in_ustack = dg < USTACK_VA_END && dt > USTACK_VA_BASE;
    assert_test!(!safe_in_ustack, "emergency safe stack overlaps USTACK");
    assert_test!(!data_in_ustack, "emergency data stack overlaps USTACK");

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

/// On a private election, not the machine's: claiming that one tells the IDT's
/// NMI stop and the TLB ack wait that a fatal panic is in flight.
pub fn test_panic_owner_election() -> TestResult {
    use slopos_ostd::panic::{PanicOwner, panic_owner_claimed, panic_owner_is};

    // The machine's own election, read-only, so the free functions under test
    // are still known to be the ones delegating to it.
    let me = slopos_arch::pcr::get_current_cpu() as u32;
    assert_test!(
        !panic_owner_claimed(),
        "the machine's fatal-panic election is claimed outside a panic"
    );
    assert_test!(
        !panic_owner_is(me),
        "this CPU is the fatal-panic owner outside a panic"
    );

    let election = PanicOwner::new();
    assert_test!(!election.claimed(), "owner claimed before any election");

    assert_test!(election.claim(7), "first claim did not win");
    assert_test!(election.claimed(), "owner not claimed after a win");
    assert_test!(election.is_owner(7), "owner mismatch after claim");
    assert_test!(!election.claim(9), "a second CPU wrongly won the election");
    assert_test!(election.is_owner(7), "owner changed after a losing claim");
    assert_test!(!election.is_owner(9), "loser reported as owner");
    assert_test!(
        !election.claim(7),
        "the winner re-claiming its own election won a second time"
    );

    election.reset_for_test();
    assert_test!(!election.claimed(), "reset did not clear the owner");
    assert_test!(election.claim(9), "a reset election refused a fresh claim");
    TestResult::Pass
}

slopos_testing::stest!(name = test_emergency_stacks_primed, suite = abort_core);
slopos_testing::stest!(name = test_emergency_stacks_disjoint, suite = abort_core);
slopos_testing::stest!(name = test_emergency_guard_classifier, suite = abort_core);
slopos_testing::stest!(name = test_panic_owner_election, suite = abort_core);
