//! Regression tests for the per-CPU exception SafeStack **data** stack.
//!
//! These pin the invariants behind the fix for the "exception handler writes
//! its `format_args!` locals onto the interrupted task's data stack →
//! supervisor #PF → recursive panic" crash:
//!
//! 1. `__safestack_pointer_address` selects the data-stack slot from the
//!    running `RSP` — task/kernel context resolves the per-task slot,
//!    IST/exception context resolves the per-CPU `ist_unsafe_sp`.
//! 2. Each online CPU's `ist_unsafe_sp` is primed into a dedicated,
//!    guard-paged exception data-stack region DISJOINT from the per-task
//!    `USTACK` region — so an exception handler can never land on a task's
//!    data stack.
//!
//! They run on a normal kernel (task) stack, so they cannot drive the IST
//! branch directly; the comprehensive suite (every kernel/COW page fault
//! runs instrumented handler code through the new resolver, and a flipped
//! range-check polarity would crash every task prologue on boot) covers the
//! live path. What these lock down is the data-structure contract a
//! regression would most plausibly break: priming, region placement, and
//! disjointness.

use slopos_mm::memory_layout_defs::{
    EXC_DSTACK_REGION_BASE, EXCEPTION_STACK_REGION_BASE, EXCEPTION_STACK_REGION_END,
    USTACK_VA_BASE, USTACK_VA_END,
};
use slopos_testing::{TestResult, assert_test};

use crate::ist_stacks::{exc_dstack_bounds_current_cpu, exc_dstack_top_current_cpu};

/// `ist_unsafe_sp` is primed to the top of the current CPU's exception data
/// stack — never zero (the uninitialised value the naked resolver would hand
/// a SafeStack prologue as a slot pointing at address 0).
pub fn test_exc_dstack_primed() -> TestResult {
    let sp = slopos_arch::pcr::local_ist_unsafe_sp();
    let (guard_start, usable_base, top) = exc_dstack_bounds_current_cpu();

    assert_test!(sp != 0, "ist_unsafe_sp is unprimed (zero)");
    assert_test!(
        sp == top,
        "ist_unsafe_sp not at the exception data-stack top"
    );
    assert_test!(
        top == exc_dstack_top_current_cpu(),
        "exc_dstack_top_current_cpu disagrees with bounds"
    );
    assert_test!(
        usable_base > guard_start,
        "guard page not below usable range"
    );
    assert_test!(
        top - usable_base >= 0x10000,
        "exception data stack smaller than 64 KiB"
    );
    TestResult::Pass
}

/// The exception data-stack region is DISJOINT from the per-task `USTACK`
/// region and from the IST *safe*-stack region — i.e. an exception handler's
/// data stack can never alias a task's data stack (the root of the original
/// crash) nor the IST safe stack it runs on.
pub fn test_exc_dstack_region_disjoint() -> TestResult {
    let (guard_start, _usable_base, top) = exc_dstack_bounds_current_cpu();

    let overlaps_ustack = guard_start < USTACK_VA_END && top > USTACK_VA_BASE;
    assert_test!(
        !overlaps_ustack,
        "exception data stack overlaps the per-task USTACK region"
    );

    let overlaps_ist_safe =
        guard_start < EXCEPTION_STACK_REGION_END && top > EXCEPTION_STACK_REGION_BASE;
    assert_test!(
        !overlaps_ist_safe,
        "exception data stack overlaps the IST safe-stack region"
    );

    assert_test!(
        guard_start >= EXC_DSTACK_REGION_BASE,
        "exception data stack below its declared region base"
    );
    TestResult::Pass
}

slopos_testing::stest!(name = test_exc_dstack_primed, suite = safestack_dstack);
slopos_testing::stest!(
    name = test_exc_dstack_region_disjoint,
    suite = safestack_dstack
);
