//! The per-CPU exception SafeStack **data** stack: every online CPU's
//! `ist_unsafe_sp` is primed into a guard-paged region that stays disjoint from
//! the per-task `USTACK` region, so a handler never lands on a task's data stack.

use slopos_mm::memory_layout_defs::{
    EXC_DSTACK_REGION_BASE, EXCEPTION_STACK_REGION_BASE, EXCEPTION_STACK_REGION_END,
    USTACK_VA_BASE, USTACK_VA_END,
};
use slopos_testing::{TestResult, assert_test};

use crate::ist_stacks::{exc_dstack_bounds_current_cpu, exc_dstack_top_current_cpu};

/// Never zero: the naked resolver would hand a SafeStack prologue a slot
/// pointing at address 0.
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

/// Disjoint from the per-task `USTACK` region and from the IST *safe*-stack
/// region, so a handler's data stack can alias neither.
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
