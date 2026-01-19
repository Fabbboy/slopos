#![no_std]

use core::ffi::{c_char, c_int};
use core::ptr;

use slopos_drivers::interrupt_test::interrupt_test_request_shutdown;
use slopos_drivers::interrupts::SUITE_SCHEDULER;
pub use slopos_drivers::interrupts::{InterruptTestConfig, Verbosity as InterruptTestVerbosity};
use slopos_lib::klog_info;

pub mod exception_tests;

pub const TESTS_MAX_SUITES: usize = 25;
const TESTS_MAX_CYCLES_PER_MS: u64 = 3_000_000;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TestSuiteResult {
    pub name: *const c_char,
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub exceptions_caught: u32,
    pub unexpected_exceptions: u32,
    pub elapsed_ms: u32,
    pub timed_out: c_int,
}

impl Default for TestSuiteResult {
    fn default() -> Self {
        Self {
            name: ptr::null(),
            total: 0,
            passed: 0,
            failed: 0,
            exceptions_caught: 0,
            unexpected_exceptions: 0,
            elapsed_ms: 0,
            timed_out: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TestSuiteDesc {
    pub name: *const c_char,
    pub mask_bit: u32,
    pub run: Option<fn(*const InterruptTestConfig, *mut TestSuiteResult) -> i32>,
}

unsafe impl Sync for TestSuiteDesc {}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TestRunSummary {
    pub suites: [TestSuiteResult; TESTS_MAX_SUITES],
    pub suite_count: usize,
    pub total_tests: u32,
    pub passed: u32,
    pub failed: u32,
    pub exceptions_caught: u32,
    pub unexpected_exceptions: u32,
    pub elapsed_ms: u32,
    pub timed_out: c_int,
}

impl Default for TestRunSummary {
    fn default() -> Self {
        Self {
            suites: [TestSuiteResult::default(); TESTS_MAX_SUITES],
            suite_count: 0,
            total_tests: 0,
            passed: 0,
            failed: 0,
            exceptions_caught: 0,
            unexpected_exceptions: 0,
            elapsed_ms: 0,
            timed_out: 0,
        }
    }
}

static mut REGISTRY: [Option<&'static TestSuiteDesc>; TESTS_MAX_SUITES] = [None; TESTS_MAX_SUITES];
static mut REGISTRY_COUNT: usize = 0;
static mut CACHED_CYCLES_PER_MS: u64 = 0;

fn registry_mut() -> *mut [Option<&'static TestSuiteDesc>; TESTS_MAX_SUITES] {
    &raw mut REGISTRY
}

fn registry_count_mut() -> *mut usize {
    &raw mut REGISTRY_COUNT
}

fn cached_cycles_per_ms_mut() -> *mut u64 {
    &raw mut CACHED_CYCLES_PER_MS
}

fn estimate_cycles_per_ms() -> u64 {
    unsafe {
        if *cached_cycles_per_ms_mut() != 0 {
            return *cached_cycles_per_ms_mut();
        }
    }

    let (max_leaf, _, _, _) = slopos_lib::cpu::cpuid(0);
    let mut cycles_per_ms = TESTS_MAX_CYCLES_PER_MS;
    if max_leaf >= 0x16 {
        let (freq_mhz, _, _, _) = slopos_lib::cpu::cpuid(0x16);
        if freq_mhz != 0 {
            cycles_per_ms = freq_mhz as u64 * 1_000;
        }
    }

    unsafe {
        *cached_cycles_per_ms_mut() = cycles_per_ms;
    }
    cycles_per_ms
}

fn cycles_to_ms(cycles: u64) -> u32 {
    let cycles_per_ms = estimate_cycles_per_ms();
    if cycles_per_ms == 0 {
        return 0;
    }
    let ms = cycles / cycles_per_ms;
    if ms > u32::MAX as u64 {
        return u32::MAX;
    }
    ms as u32
}

fn fill_summary_from_result(summary: &mut TestRunSummary, res: &TestSuiteResult) {
    summary.total_tests = summary.total_tests.saturating_add(res.total);
    summary.passed = summary.passed.saturating_add(res.passed);
    summary.failed = summary.failed.saturating_add(res.failed);
    summary.exceptions_caught = summary
        .exceptions_caught
        .saturating_add(res.exceptions_caught);
    summary.unexpected_exceptions = summary
        .unexpected_exceptions
        .saturating_add(res.unexpected_exceptions);
    summary.elapsed_ms = summary.elapsed_ms.saturating_add(res.elapsed_ms);
    if res.timed_out != 0 {
        summary.timed_out = 1;
    }
}

pub fn tests_reset_registry() {
    unsafe {
        (*registry_mut()).iter_mut().for_each(|slot| *slot = None);
        *registry_count_mut() = 0;
    }
}

pub fn tests_register_suite(desc: *const TestSuiteDesc) -> i32 {
    if desc.is_null() {
        return -1;
    }
    let desc_ref = unsafe { &*desc };
    if desc_ref.run.is_none() {
        return -1;
    }
    unsafe {
        if *registry_count_mut() >= TESTS_MAX_SUITES {
            return -1;
        }
        (*registry_mut())[*registry_count_mut()] = Some(desc_ref);
        *registry_count_mut() += 1;
    }
    0
}

pub fn tests_register_system_suites() {
    suites::register_system_suites();
}

pub fn tests_run_all(config: *const InterruptTestConfig, summary: *mut TestRunSummary) -> i32 {
    if config.is_null() {
        return -1;
    }

    let mut local_summary = TestRunSummary::default();
    let summary = if summary.is_null() {
        &mut local_summary
    } else {
        unsafe {
            *summary = TestRunSummary::default();
            &mut *summary
        }
    };

    let cfg = unsafe { &*config };
    if !cfg.enabled {
        klog_info!("TESTS: Harness disabled\n");
        return 0;
    }

    klog_info!("TESTS: Starting test suites\n");

    let mut desc_list: [Option<&'static TestSuiteDesc>; TESTS_MAX_SUITES] =
        [None; TESTS_MAX_SUITES];
    let mut desc_count = unsafe { *registry_count_mut() };
    if desc_count > TESTS_MAX_SUITES {
        desc_count = TESTS_MAX_SUITES;
    }
    for i in 0..desc_count {
        desc_list[i] = unsafe { (*registry_mut())[i] };
    }

    let start_cycles = slopos_lib::tsc::rdtsc();
    for (idx, entry) in desc_list.iter().enumerate().take(desc_count) {
        let Some(desc) = entry else { continue };

        if (cfg.suite_mask & desc.mask_bit) == 0 {
            continue;
        }

        let mut res = TestSuiteResult::default();
        res.name = desc.name;

        if let Some(run) = desc.run {
            let _ = run(cfg, &mut res);
        }

        if summary.suite_count < TESTS_MAX_SUITES {
            summary.suites[summary.suite_count] = res;
            summary.suite_count += 1;
        }

        klog_info!(
            "SUITE{} total={} pass={} fail={} elapsed={}ms\n",
            idx as u32,
            res.total,
            res.passed,
            res.failed,
            res.elapsed_ms,
        );
        fill_summary_from_result(summary, &res);
    }
    let end_cycles = slopos_lib::tsc::rdtsc();
    let overall_ms = cycles_to_ms(end_cycles.wrapping_sub(start_cycles));
    if overall_ms > summary.elapsed_ms {
        summary.elapsed_ms = overall_ms;
    }

    klog_info!(
        "TESTS SUMMARY: total={} passed={} failed={} elapsed_ms={}\n",
        summary.total_tests,
        summary.passed,
        summary.failed,
        summary.elapsed_ms,
    );

    if summary.failed == 0 { 0 } else { -1 }
}

pub fn tests_request_shutdown(failed: i32) {
    interrupt_test_request_shutdown(failed);
}

mod suites {
    use super::*;

    const VM_NAME: &[u8] = b"vm\0";
    const HEAP_NAME: &[u8] = b"heap\0";
    const EXT2_NAME: &[u8] = b"ext2\0";
    const PRIVSEP_NAME: &[u8] = b"privsep\0";
    const FPU_NAME: &[u8] = b"fpu_sse\0";
    const PAGE_ALLOC_NAME: &[u8] = b"page_alloc\0";
    const HEAP_EXT_NAME: &[u8] = b"heap_ext\0";
    const PAGING_NAME: &[u8] = b"paging\0";
    const RING_BUF_NAME: &[u8] = b"ring_buf\0";
    const SPINLOCK_NAME: &[u8] = b"spinlock\0";
    const SHM_NAME: &[u8] = b"shm\0";
    const RIGOROUS_NAME: &[u8] = b"rigorous\0";
    const PROCESS_VM_NAME: &[u8] = b"process_vm\0";
    const SCHED_CORE_NAME: &[u8] = b"sched_core\0";
    const DEMAND_PAGING_NAME: &[u8] = b"demand_paging\0";
    const OOM_NAME: &[u8] = b"oom\0";
    const COW_EDGE_NAME: &[u8] = b"cow_edge\0";
    const SYSCALL_VALID_NAME: &[u8] = b"syscall_valid\0";
    const EXCEPTION_NAME: &[u8] = b"exception\0";
    const EXEC_NAME: &[u8] = b"exec\0";
    const IRQ_NAME: &[u8] = b"irq\0";
    const IOAPIC_NAME: &[u8] = b"ioapic\0";
    const CONTEXT_NAME: &[u8] = b"context\0";

    fn measure_elapsed_ms(start: u64, end: u64) -> u32 {
        super::cycles_to_ms(end.wrapping_sub(start))
    }

    fn fill_simple_result(
        out: *mut TestSuiteResult,
        name: &[u8],
        total: u32,
        passed: u32,
        elapsed_ms: u32,
    ) {
        if let Some(out_ref) = unsafe { out.as_mut() } {
            out_ref.name = name.as_ptr() as *const c_char;
            out_ref.total = total;
            out_ref.passed = passed;
            out_ref.failed = total.saturating_sub(passed);
            out_ref.exceptions_caught = 0;
            out_ref.unexpected_exceptions = 0;
            out_ref.elapsed_ms = elapsed_ms;
            out_ref.timed_out = 0;
        }
    }

    macro_rules! run_test {
        ($passed:expr, $total:expr, $test_fn:expr) => {{
            $total += 1;
            if $test_fn() == 0 {
                $passed += 1;
            }
        }};
    }

    use slopos_mm::tests::{
        test_alloc_free_cycles_no_leak, test_cow_clone_modify_both, test_cow_fault_handling,
        test_cow_handle_invalid_address, test_cow_handle_not_cow_page,
        test_cow_handle_null_pagedir, test_cow_multi_ref_copy, test_cow_multiple_clones,
        test_cow_no_collateral_damage, test_cow_not_present_not_cow, test_cow_page_boundary,
        test_cow_page_isolation, test_cow_read_not_cow_fault, test_cow_single_ref_upgrade,
        test_demand_double_fault, test_demand_fault_no_vma, test_demand_fault_non_lazy_vma,
        test_demand_fault_present_page, test_demand_fault_valid_lazy_vma,
        test_demand_handle_no_vma, test_demand_handle_null_page_dir,
        test_demand_handle_page_boundary, test_demand_handle_permission_denied,
        test_demand_handle_success, test_demand_invalid_process_id, test_demand_multiple_faults,
        test_demand_permission_allow_read, test_demand_permission_allow_write,
        test_demand_permission_deny_exec, test_demand_permission_deny_user_kernel,
        test_demand_permission_deny_write_ro, test_dma_allocation_exhaustion,
        test_heap_alloc_pressure, test_heap_alloc_zero, test_heap_boundary_write,
        test_heap_double_free_defensive, test_heap_expansion_under_pressure,
        test_heap_fragmentation_behind_head, test_heap_free_list_search, test_heap_kfree_null,
        test_heap_kzalloc_zeroed, test_heap_large_alloc, test_heap_large_block_integrity,
        test_heap_medium_alloc, test_heap_no_overlap, test_heap_small_alloc, test_heap_stats,
        test_heap_stress_cycles, test_irqmutex_basic, test_irqmutex_mutation,
        test_irqmutex_try_lock, test_kzalloc_zeroed_under_pressure, test_multiorder_alloc_failure,
        test_multiple_process_vms, test_page_alloc_fragmentation,
        test_page_alloc_fragmentation_oom, test_page_alloc_free_cycle, test_page_alloc_free_null,
        test_page_alloc_multi_order, test_page_alloc_multipage_integrity,
        test_page_alloc_no_stale_data, test_page_alloc_refcount, test_page_alloc_single,
        test_page_alloc_stats, test_page_alloc_until_oom, test_page_alloc_write_verify,
        test_page_alloc_zero_full_page, test_page_alloc_zeroed, test_paging_cow_kernel,
        test_paging_get_kernel_dir, test_paging_user_accessible_kernel, test_paging_virt_to_phys,
        test_process_heap_expansion_oom, test_process_vm_alloc_and_access,
        test_process_vm_brk_expansion, test_process_vm_counter_reset,
        test_process_vm_create_destroy_memory, test_process_vm_creation_pressure,
        test_process_vm_slot_reuse, test_refcount_during_oom, test_ring_buffer_basic,
        test_ring_buffer_capacity, test_ring_buffer_empty_pop, test_ring_buffer_fifo,
        test_ring_buffer_full, test_ring_buffer_overwrite, test_ring_buffer_reset,
        test_ring_buffer_wrap, test_shm_create_destroy, test_shm_create_excessive_size,
        test_shm_create_zero_size, test_shm_destroy_non_owner, test_shm_invalid_token,
        test_shm_refcount, test_shm_surface_attach, test_shm_surface_attach_too_small,
        test_spinlock_basic, test_spinlock_init, test_spinlock_irqsave, test_vma_flags_retrieval,
        test_zero_flag_under_pressure,
    };

    use slopos_core::sched_tests::{
        test_create_conflicting_flags, test_create_max_tasks, test_create_null_entry,
        test_create_null_name, test_create_over_max_tasks, test_double_terminate,
        test_find_invalid_id, test_get_info_null_output, test_idle_priority_last,
        test_interleaved_operations, test_many_same_priority_tasks, test_priority_ordering,
        test_rapid_create_destroy_cycle, test_schedule_duplicate_task, test_schedule_null_task,
        test_schedule_to_empty_queue, test_schedule_while_disabled, test_scheduler_starts_disabled,
        test_state_transition_invalid_blocked_to_running,
        test_state_transition_invalid_terminated_to_running,
        test_state_transition_ready_to_running, test_state_transition_running_to_blocked,
        test_terminate_invalid_id, test_terminate_nonexistent_id, test_timer_tick_decrements_slice,
        test_timer_tick_no_current_task, test_unschedule_not_in_queue,
    };

    use crate::exception_tests::run_exception_tests;
    use slopos_core::run_context_tests;
    use slopos_core::run_exec_tests;
    use slopos_core::run_irq_tests;
    use slopos_core::run_syscall_validation_tests;
    use slopos_drivers::ioapic_tests::run_ioapic_tests;

    fn run_vm_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        run_test!(passed, total, test_process_vm_slot_reuse);
        run_test!(passed, total, test_process_vm_counter_reset);

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, VM_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_heap_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        run_test!(passed, total, test_heap_free_list_search);
        run_test!(passed, total, test_heap_fragmentation_behind_head);

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, HEAP_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_ext2_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let total = 5u32;
        let passed = slopos_fs::tests::run_ext2_tests().max(0) as u32;
        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, EXT2_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_privsep_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let result = slopos_core::run_privilege_separation_invariant_test();
        let passed = if result == 0 { 1 } else { 0 };
        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, PRIVSEP_NAME, 1, passed, elapsed);
        if result == 0 { 0 } else { -1 }
    }

    fn run_fpu_sse_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        use core::arch::x86_64::{__m128i, _mm_set_epi64x, _mm_storeu_si128};

        let start = slopos_lib::tsc::rdtsc();
        let total = 2u32;
        let mut passed = 0u32;

        let pattern_lo: i64 = 0x_DEAD_BEEF_CAFE_BABE_u64 as i64;
        let pattern_hi: i64 = 0x_1234_5678_9ABC_DEF0_u64 as i64;
        let expected = unsafe { _mm_set_epi64x(pattern_hi, pattern_lo) };

        let readback: __m128i;
        unsafe {
            core::arch::asm!(
                "movdqa {tmp}, {src}",
                "movdqa xmm0, {tmp}",
                tmp = out(xmm_reg) _,
                src = in(xmm_reg) expected,
            );
            core::arch::asm!(
                "movdqa {dst}, xmm0",
                dst = out(xmm_reg) readback,
            );
        }

        let mut result = [0u8; 16];
        let mut expected_bytes = [0u8; 16];
        unsafe {
            _mm_storeu_si128(result.as_mut_ptr() as *mut __m128i, readback);
            _mm_storeu_si128(expected_bytes.as_mut_ptr() as *mut __m128i, expected);
        }
        if result == expected_bytes {
            passed += 1;
        }

        let pattern2_lo: i64 = 0x_FFFF_0000_AAAA_5555_u64 as i64;
        let pattern2_hi: i64 = 0x_0123_4567_89AB_CDEF_u64 as i64;
        let pattern2 = unsafe { _mm_set_epi64x(pattern2_hi, pattern2_lo) };

        let readback2: __m128i;
        unsafe {
            core::arch::asm!(
                "movdqa xmm1, {src}",
                "movdqa {dst}, xmm1",
                src = in(xmm_reg) pattern2,
                dst = out(xmm_reg) readback2,
            );
        }

        let mut expected2_bytes = [0u8; 16];
        unsafe {
            _mm_storeu_si128(result.as_mut_ptr() as *mut __m128i, readback2);
            _mm_storeu_si128(expected2_bytes.as_mut_ptr() as *mut __m128i, pattern2);
        }
        if result == expected2_bytes {
            passed += 1;
        }

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, FPU_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_page_alloc_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        run_test!(passed, total, test_page_alloc_single);
        run_test!(passed, total, test_page_alloc_multi_order);
        run_test!(passed, total, test_page_alloc_free_cycle);
        run_test!(passed, total, test_page_alloc_zeroed);
        run_test!(passed, total, test_page_alloc_refcount);
        run_test!(passed, total, test_page_alloc_stats);
        run_test!(passed, total, test_page_alloc_free_null);
        run_test!(passed, total, test_page_alloc_fragmentation);

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, PAGE_ALLOC_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_heap_ext_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        run_test!(passed, total, test_heap_small_alloc);
        run_test!(passed, total, test_heap_medium_alloc);
        run_test!(passed, total, test_heap_large_alloc);
        run_test!(passed, total, test_heap_kzalloc_zeroed);
        run_test!(passed, total, test_heap_kfree_null);
        run_test!(passed, total, test_heap_alloc_zero);
        run_test!(passed, total, test_heap_stats);

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, HEAP_EXT_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_paging_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        run_test!(passed, total, test_paging_virt_to_phys);
        run_test!(passed, total, test_paging_get_kernel_dir);
        run_test!(passed, total, test_paging_user_accessible_kernel);
        run_test!(passed, total, test_paging_cow_kernel);

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, PAGING_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_ring_buf_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        run_test!(passed, total, test_ring_buffer_basic);
        run_test!(passed, total, test_ring_buffer_fifo);
        run_test!(passed, total, test_ring_buffer_empty_pop);
        run_test!(passed, total, test_ring_buffer_full);
        run_test!(passed, total, test_ring_buffer_overwrite);
        run_test!(passed, total, test_ring_buffer_wrap);
        run_test!(passed, total, test_ring_buffer_reset);
        run_test!(passed, total, test_ring_buffer_capacity);

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, RING_BUF_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_spinlock_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        run_test!(passed, total, test_spinlock_basic);
        run_test!(passed, total, test_spinlock_irqsave);
        run_test!(passed, total, test_spinlock_init);
        run_test!(passed, total, test_irqmutex_basic);
        run_test!(passed, total, test_irqmutex_mutation);
        run_test!(passed, total, test_irqmutex_try_lock);

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, SPINLOCK_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_shm_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        run_test!(passed, total, test_shm_create_destroy);
        run_test!(passed, total, test_shm_create_zero_size);
        run_test!(passed, total, test_shm_create_excessive_size);
        run_test!(passed, total, test_shm_destroy_non_owner);
        run_test!(passed, total, test_shm_refcount);
        run_test!(passed, total, test_shm_invalid_token);
        run_test!(passed, total, test_shm_surface_attach);
        run_test!(passed, total, test_shm_surface_attach_too_small);

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, SHM_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_rigorous_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        run_test!(passed, total, test_page_alloc_write_verify);
        run_test!(passed, total, test_page_alloc_zero_full_page);
        run_test!(passed, total, test_page_alloc_no_stale_data);
        run_test!(passed, total, test_heap_boundary_write);
        run_test!(passed, total, test_heap_no_overlap);
        run_test!(passed, total, test_heap_double_free_defensive);
        run_test!(passed, total, test_heap_large_block_integrity);
        run_test!(passed, total, test_heap_stress_cycles);
        run_test!(passed, total, test_page_alloc_multipage_integrity);

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, RIGOROUS_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_process_vm_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        run_test!(passed, total, test_process_vm_create_destroy_memory);
        run_test!(passed, total, test_process_vm_alloc_and_access);
        run_test!(passed, total, test_process_vm_brk_expansion);
        run_test!(passed, total, test_cow_page_isolation);
        run_test!(passed, total, test_cow_fault_handling);
        run_test!(passed, total, test_multiple_process_vms);
        run_test!(passed, total, test_vma_flags_retrieval);

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, PROCESS_VM_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_sched_core_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        run_test!(passed, total, test_state_transition_ready_to_running);
        run_test!(passed, total, test_state_transition_running_to_blocked);
        run_test!(
            passed,
            total,
            test_state_transition_invalid_terminated_to_running
        );
        run_test!(
            passed,
            total,
            test_state_transition_invalid_blocked_to_running
        );
        run_test!(passed, total, test_create_max_tasks);
        run_test!(passed, total, test_create_over_max_tasks);
        run_test!(passed, total, test_rapid_create_destroy_cycle);
        run_test!(passed, total, test_schedule_to_empty_queue);
        run_test!(passed, total, test_schedule_duplicate_task);
        run_test!(passed, total, test_schedule_null_task);
        run_test!(passed, total, test_unschedule_not_in_queue);
        run_test!(passed, total, test_priority_ordering);
        run_test!(passed, total, test_idle_priority_last);
        run_test!(passed, total, test_timer_tick_no_current_task);
        run_test!(passed, total, test_timer_tick_decrements_slice);
        run_test!(passed, total, test_terminate_invalid_id);
        run_test!(passed, total, test_terminate_nonexistent_id);
        run_test!(passed, total, test_double_terminate);
        run_test!(passed, total, test_find_invalid_id);
        run_test!(passed, total, test_get_info_null_output);
        run_test!(passed, total, test_create_null_entry);
        run_test!(passed, total, test_create_conflicting_flags);
        run_test!(passed, total, test_create_null_name);
        run_test!(passed, total, test_scheduler_starts_disabled);
        run_test!(passed, total, test_schedule_while_disabled);
        run_test!(passed, total, test_many_same_priority_tasks);
        run_test!(passed, total, test_interleaved_operations);

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, SCHED_CORE_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_demand_paging_suite(
        _config: *const InterruptTestConfig,
        out: *mut TestSuiteResult,
    ) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        run_test!(passed, total, test_demand_fault_present_page);
        run_test!(passed, total, test_demand_fault_no_vma);
        run_test!(passed, total, test_demand_fault_non_lazy_vma);
        run_test!(passed, total, test_demand_fault_valid_lazy_vma);
        run_test!(passed, total, test_demand_permission_deny_write_ro);
        run_test!(passed, total, test_demand_permission_deny_user_kernel);
        run_test!(passed, total, test_demand_permission_deny_exec);
        run_test!(passed, total, test_demand_permission_allow_read);
        run_test!(passed, total, test_demand_permission_allow_write);
        run_test!(passed, total, test_demand_handle_null_page_dir);
        run_test!(passed, total, test_demand_handle_no_vma);
        run_test!(passed, total, test_demand_handle_success);
        run_test!(passed, total, test_demand_handle_permission_denied);
        run_test!(passed, total, test_demand_handle_page_boundary);
        run_test!(passed, total, test_demand_multiple_faults);
        run_test!(passed, total, test_demand_double_fault);
        run_test!(passed, total, test_demand_invalid_process_id);

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, DEMAND_PAGING_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_oom_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        run_test!(passed, total, test_page_alloc_until_oom);
        run_test!(passed, total, test_page_alloc_fragmentation_oom);
        run_test!(passed, total, test_dma_allocation_exhaustion);
        run_test!(passed, total, test_heap_alloc_pressure);
        run_test!(passed, total, test_process_vm_creation_pressure);
        run_test!(passed, total, test_heap_expansion_under_pressure);
        run_test!(passed, total, test_zero_flag_under_pressure);
        run_test!(passed, total, test_kzalloc_zeroed_under_pressure);
        run_test!(passed, total, test_alloc_free_cycles_no_leak);
        run_test!(passed, total, test_multiorder_alloc_failure);
        run_test!(passed, total, test_process_heap_expansion_oom);
        run_test!(passed, total, test_refcount_during_oom);

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, OOM_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_cow_edge_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        run_test!(passed, total, test_cow_read_not_cow_fault);
        run_test!(passed, total, test_cow_not_present_not_cow);
        run_test!(passed, total, test_cow_handle_null_pagedir);
        run_test!(passed, total, test_cow_handle_not_cow_page);
        run_test!(passed, total, test_cow_single_ref_upgrade);
        run_test!(passed, total, test_cow_multi_ref_copy);
        run_test!(passed, total, test_cow_page_boundary);
        run_test!(passed, total, test_cow_clone_modify_both);
        run_test!(passed, total, test_cow_multiple_clones);
        run_test!(passed, total, test_cow_no_collateral_damage);
        run_test!(passed, total, test_cow_handle_invalid_address);

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, COW_EDGE_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_syscall_valid_suite(
        _config: *const InterruptTestConfig,
        out: *mut TestSuiteResult,
    ) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let (passed, total) = run_syscall_validation_tests();
        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, SYSCALL_VALID_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_exception_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let (passed, total) = run_exception_tests();
        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, EXCEPTION_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_exec_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let (passed, total) = run_exec_tests();
        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, EXEC_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_irq_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let (passed, total) = run_irq_tests();
        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, IRQ_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_ioapic_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let (passed, total) = run_ioapic_tests();
        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, IOAPIC_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_context_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let (passed, total) = run_context_tests();
        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, CONTEXT_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    pub fn register_system_suites() {
        static VM_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: VM_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_vm_suite),
        };
        static HEAP_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: HEAP_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_heap_suite),
        };
        static EXT2_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: EXT2_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_ext2_suite),
        };
        static PRIVSEP_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: PRIVSEP_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_privsep_suite),
        };
        static FPU_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: FPU_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_fpu_sse_suite),
        };
        static PAGE_ALLOC_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: PAGE_ALLOC_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_page_alloc_suite),
        };
        static HEAP_EXT_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: HEAP_EXT_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_heap_ext_suite),
        };
        static PAGING_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: PAGING_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_paging_suite),
        };
        static RING_BUF_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: RING_BUF_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_ring_buf_suite),
        };
        static SPINLOCK_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: SPINLOCK_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_spinlock_suite),
        };
        static SHM_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: SHM_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_shm_suite),
        };
        static RIGOROUS_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: RIGOROUS_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_rigorous_suite),
        };
        static PROCESS_VM_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: PROCESS_VM_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_process_vm_suite),
        };
        static SCHED_CORE_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: SCHED_CORE_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_sched_core_suite),
        };
        static DEMAND_PAGING_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: DEMAND_PAGING_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_demand_paging_suite),
        };
        static OOM_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: OOM_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_oom_suite),
        };
        static COW_EDGE_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: COW_EDGE_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_cow_edge_suite),
        };
        static SYSCALL_VALID_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: SYSCALL_VALID_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_syscall_valid_suite),
        };
        static EXCEPTION_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: EXCEPTION_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_exception_suite),
        };
        static EXEC_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: EXEC_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_exec_suite),
        };
        static IRQ_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: IRQ_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_irq_suite),
        };
        static IOAPIC_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: IOAPIC_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_ioapic_suite),
        };
        static CONTEXT_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
            name: CONTEXT_NAME.as_ptr() as *const c_char,
            mask_bit: SUITE_SCHEDULER,
            run: Some(run_context_suite),
        };

        let _ = tests_register_suite(&VM_SUITE_DESC);
        let _ = tests_register_suite(&HEAP_SUITE_DESC);
        let _ = tests_register_suite(&EXT2_SUITE_DESC);
        let _ = tests_register_suite(&PRIVSEP_SUITE_DESC);
        let _ = tests_register_suite(&FPU_SUITE_DESC);
        let _ = tests_register_suite(&PAGE_ALLOC_SUITE_DESC);
        let _ = tests_register_suite(&HEAP_EXT_SUITE_DESC);
        let _ = tests_register_suite(&PAGING_SUITE_DESC);
        let _ = tests_register_suite(&RING_BUF_SUITE_DESC);
        let _ = tests_register_suite(&SPINLOCK_SUITE_DESC);
        let _ = tests_register_suite(&SHM_SUITE_DESC);
        let _ = tests_register_suite(&RIGOROUS_SUITE_DESC);
        let _ = tests_register_suite(&PROCESS_VM_SUITE_DESC);
        let _ = tests_register_suite(&SCHED_CORE_SUITE_DESC);
        let _ = tests_register_suite(&DEMAND_PAGING_SUITE_DESC);
        let _ = tests_register_suite(&OOM_SUITE_DESC);
        let _ = tests_register_suite(&COW_EDGE_SUITE_DESC);
        let _ = tests_register_suite(&SYSCALL_VALID_SUITE_DESC);
        let _ = tests_register_suite(&EXCEPTION_SUITE_DESC);
        let _ = tests_register_suite(&EXEC_SUITE_DESC);
        let _ = tests_register_suite(&IRQ_SUITE_DESC);
        let _ = tests_register_suite(&IOAPIC_SUITE_DESC);
        let _ = tests_register_suite(&CONTEXT_SUITE_DESC);
    }
}
