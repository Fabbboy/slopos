//! Per-CPU IST, exception and emergency stack mappings. A hole in one triple-faults
//! rather than reporting anything, so the mappings are checked here.

use slopos_abi::addr::VirtAddr;
use slopos_arch::get_cpu_count;
use slopos_mm::memory_layout_defs::{
    EMERGENCY_DSTACK_PAGES, EMERGENCY_SAFE_STACK_PAGES, EXC_DSTACK_PAGES, EXCEPTION_STACK_PAGES,
};
use slopos_mm::paging::{is_mapped, virt_to_phys};
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_testing::{TestResult, assert_test};

use crate::ist_stacks::{
    IST_STACK_COUNT, emergency_dstack_bounds_for_cpu, emergency_safe_bounds_for_cpu,
    exc_dstack_bounds_for_cpu, stack_bounds_for_cpu,
};

/// `[guard_start, usable_base)` must be unmapped; the `usable_pages` pages above
/// it must all resolve.
fn check_guarded_region(
    cpu_id: usize,
    what: &str,
    guard_start: u64,
    usable_base: u64,
    usable_pages: u64,
) -> TestResult {
    let guard_pages = (usable_base - guard_start) / PAGE_SIZE_4KB;
    assert_test!(guard_pages > 0, "CPU{} {} has no guard page", cpu_id, what);

    for page in 0..guard_pages {
        let addr = VirtAddr::new(guard_start + page * PAGE_SIZE_4KB);
        assert_test!(
            is_mapped(addr) == 0,
            "CPU{} {} guard page 0x{:x} is mapped",
            cpu_id,
            what,
            addr.as_u64()
        );
    }

    for page in 0..usable_pages {
        let addr = VirtAddr::new(usable_base + page * PAGE_SIZE_4KB);
        assert_test!(
            !virt_to_phys(addr).is_null(),
            "CPU{} {} page 0x{:x} is unmapped",
            cpu_id,
            what,
            addr.as_u64()
        );
    }

    TestResult::Pass
}

pub fn test_ist_stacks_mapped_on_every_cpu() -> TestResult {
    let cpu_count = get_cpu_count();
    assert_test!(cpu_count >= 1, "no CPU reported a control region");

    for cpu_id in 0..cpu_count {
        for idx in 0..IST_STACK_COUNT {
            let (guard_start, guard_end, _stack_base, _stack_top) =
                stack_bounds_for_cpu(cpu_id, idx);
            let outcome = check_guarded_region(
                cpu_id,
                "IST stack",
                guard_start,
                guard_end,
                EXCEPTION_STACK_PAGES,
            );
            if outcome != TestResult::Pass {
                return outcome;
            }
        }
    }
    TestResult::Pass
}

/// The stack an instrumented exception handler writes address-taken locals to.
pub fn test_exception_data_stacks_mapped_on_every_cpu() -> TestResult {
    for cpu_id in 0..get_cpu_count() {
        let (guard_start, usable_base, _top) = exc_dstack_bounds_for_cpu(cpu_id);
        let outcome = check_guarded_region(
            cpu_id,
            "exception data stack",
            guard_start,
            usable_base,
            EXC_DSTACK_PAGES,
        );
        if outcome != TestResult::Pass {
            return outcome;
        }
    }
    TestResult::Pass
}

/// The stacks the fatal-fault trampoline switches to before any panic formatting.
pub fn test_emergency_stacks_mapped_on_every_cpu() -> TestResult {
    for cpu_id in 0..get_cpu_count() {
        let (safe_guard, safe_base, _safe_top) = emergency_safe_bounds_for_cpu(cpu_id);
        let outcome = check_guarded_region(
            cpu_id,
            "emergency safe stack",
            safe_guard,
            safe_base,
            EMERGENCY_SAFE_STACK_PAGES,
        );
        if outcome != TestResult::Pass {
            return outcome;
        }

        let (data_guard, data_base, _data_top) = emergency_dstack_bounds_for_cpu(cpu_id);
        let outcome = check_guarded_region(
            cpu_id,
            "emergency data stack",
            data_guard,
            data_base,
            EMERGENCY_DSTACK_PAGES,
        );
        if outcome != TestResult::Pass {
            return outcome;
        }
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_ist_stacks_mapped_on_every_cpu,
    suite = ist_premap
);
slopos_testing::stest!(
    name = test_exception_data_stacks_mapped_on_every_cpu,
    suite = ist_premap
);
slopos_testing::stest!(
    name = test_emergency_stacks_mapped_on_every_cpu,
    suite = ist_premap
);
