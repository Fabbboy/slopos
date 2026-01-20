#![no_std]

use core::ffi::c_char;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_drivers::interrupt_test::interrupt_test_request_shutdown;
use slopos_drivers::interrupts::SUITE_SCHEDULER;
pub use slopos_drivers::interrupts::{InterruptTestConfig, Verbosity as InterruptTestVerbosity};
pub use slopos_lib::testing::{
    HARNESS_MAX_SUITES, TestSuiteDesc, TestSuiteResult, measure_elapsed_ms,
};
use slopos_lib::{StateFlag, define_test_suite, klog_info, register_test_suites};

pub mod exception_tests;

pub const TESTS_MAX_SUITES: usize = HARNESS_MAX_SUITES;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TestRunSummary {
    pub suites: [TestSuiteResult; TESTS_MAX_SUITES],
    pub suite_count: usize,
    pub total_tests: u32,
    pub passed: u32,
    pub failed: u32,
    pub exceptions_caught: u32,
    pub unexpected_exceptions: u32,
    pub elapsed_ms: u32,
    pub timed_out: core::ffi::c_int,
}

impl TestRunSummary {
    fn add_suite_result(&mut self, result: &TestSuiteResult) {
        self.total_tests = self.total_tests.saturating_add(result.total);
        self.passed = self.passed.saturating_add(result.passed);
        self.failed = self.failed.saturating_add(result.failed);
        self.exceptions_caught = self
            .exceptions_caught
            .saturating_add(result.exceptions_caught);
        self.unexpected_exceptions = self
            .unexpected_exceptions
            .saturating_add(result.unexpected_exceptions);
        self.elapsed_ms = self.elapsed_ms.saturating_add(result.elapsed_ms);
        if result.timed_out != 0 {
            self.timed_out = 1;
        }
    }
}

static mut REGISTRY: [Option<&'static TestSuiteDesc>; TESTS_MAX_SUITES] = [None; TESTS_MAX_SUITES];
static mut REGISTRY_COUNT: usize = 0;
static PANIC_SEEN: StateFlag = StateFlag::new();
static PANIC_REPORTED: AtomicBool = AtomicBool::new(false);

fn registry_mut() -> *mut [Option<&'static TestSuiteDesc>; TESTS_MAX_SUITES] {
    &raw mut REGISTRY
}

fn registry_count_mut() -> *mut usize {
    &raw mut REGISTRY_COUNT
}

pub fn tests_reset_registry() {
    unsafe {
        (*registry_mut()).iter_mut().for_each(|slot| *slot = None);
        *registry_count_mut() = 0;
    }
    PANIC_SEEN.set_inactive();
    PANIC_REPORTED.store(false, Ordering::Relaxed);
}

pub fn tests_register_suite(desc: &'static TestSuiteDesc) -> i32 {
    if desc.run.is_none() {
        return -1;
    }
    unsafe {
        if *registry_count_mut() >= TESTS_MAX_SUITES {
            return -1;
        }
        (*registry_mut())[*registry_count_mut()] = Some(desc);
        *registry_count_mut() += 1;
    }
    0
}

pub fn tests_register_system_suites() {
    suites::register_all();
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
        if PANIC_SEEN.is_active() {
            summary.unexpected_exceptions = summary.unexpected_exceptions.saturating_add(1);
            summary.failed = summary.failed.saturating_add(1);
            if !PANIC_REPORTED.swap(true, Ordering::Relaxed) {
                klog_info!("TESTS: panic flagged, stopping suite execution\n");
            }
            break;
        }

        let Some(desc) = entry else { continue };

        if (cfg.suite_mask & desc.mask_bit) == 0 {
            continue;
        }

        let suite_start = slopos_lib::tsc::rdtsc();
        let mut res = TestSuiteResult::default();
        res.name = desc.name;

        if let Some(run) = desc.run {
            let config_ptr = config as *const ();
            let _ = run(config_ptr, &mut res);
        }

        if PANIC_SEEN.is_active() {
            res.unexpected_exceptions = res.unexpected_exceptions.saturating_add(1);
            res.failed = res.failed.saturating_add(1);
        }

        if cfg.timeout_ms != 0 {
            let elapsed = measure_elapsed_ms(suite_start, slopos_lib::tsc::rdtsc());
            if elapsed > cfg.timeout_ms {
                res.timed_out = 1;
                res.failed = res.failed.saturating_add(1);
                if !PANIC_REPORTED.swap(true, Ordering::Relaxed) {
                    klog_info!("TESTS: suite timeout exceeded\n");
                }
            }
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
        summary.add_suite_result(&res);
    }
    let end_cycles = slopos_lib::tsc::rdtsc();
    let overall_ms = measure_elapsed_ms(start_cycles, end_cycles);
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

pub fn tests_mark_panic() {
    PANIC_SEEN.set_active();
    if !PANIC_REPORTED.swap(true, Ordering::Relaxed) {
        klog_info!("TESTS: panic observed\n");
    }
}

mod suites {
    use super::*;
    use slopos_lib::testing::HarnessConfig;

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
        test_shm_mapping_overflow, test_shm_refcount, test_shm_surface_attach,
        test_shm_surface_attach_overflow, test_shm_surface_attach_too_small, test_spinlock_basic,
        test_spinlock_init, test_spinlock_irqsave, test_vma_flags_retrieval,
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

    define_test_suite!(
        vm,
        SUITE_SCHEDULER,
        [test_process_vm_slot_reuse, test_process_vm_counter_reset,]
    );

    define_test_suite!(
        heap,
        SUITE_SCHEDULER,
        [
            test_heap_free_list_search,
            test_heap_fragmentation_behind_head,
        ]
    );

    define_test_suite!(
        ext2,
        SUITE_SCHEDULER,
        slopos_fs::tests::run_ext2_tests_summary
    );

    define_test_suite!(
        privsep,
        SUITE_SCHEDULER,
        slopos_core::run_privilege_separation_invariant_test,
        single
    );

    define_test_suite!(
        page_alloc,
        SUITE_SCHEDULER,
        [
            test_page_alloc_single,
            test_page_alloc_multi_order,
            test_page_alloc_free_cycle,
            test_page_alloc_zeroed,
            test_page_alloc_refcount,
            test_page_alloc_stats,
            test_page_alloc_free_null,
            test_page_alloc_fragmentation,
        ]
    );

    define_test_suite!(
        heap_ext,
        SUITE_SCHEDULER,
        [
            test_heap_small_alloc,
            test_heap_medium_alloc,
            test_heap_large_alloc,
            test_heap_kzalloc_zeroed,
            test_heap_kfree_null,
            test_heap_alloc_zero,
            test_heap_stats,
        ]
    );

    define_test_suite!(
        paging,
        SUITE_SCHEDULER,
        [
            test_paging_virt_to_phys,
            test_paging_get_kernel_dir,
            test_paging_user_accessible_kernel,
            test_paging_cow_kernel,
        ]
    );

    define_test_suite!(
        ring_buf,
        SUITE_SCHEDULER,
        [
            test_ring_buffer_basic,
            test_ring_buffer_fifo,
            test_ring_buffer_empty_pop,
            test_ring_buffer_full,
            test_ring_buffer_overwrite,
            test_ring_buffer_wrap,
            test_ring_buffer_reset,
            test_ring_buffer_capacity,
        ]
    );

    define_test_suite!(
        spinlock,
        SUITE_SCHEDULER,
        [
            test_spinlock_basic,
            test_spinlock_irqsave,
            test_spinlock_init,
            test_irqmutex_basic,
            test_irqmutex_mutation,
            test_irqmutex_try_lock,
        ]
    );

    define_test_suite!(
        shm,
        SUITE_SCHEDULER,
        [
            test_shm_create_destroy,
            test_shm_create_zero_size,
            test_shm_create_excessive_size,
            test_shm_destroy_non_owner,
            test_shm_refcount,
            test_shm_invalid_token,
            test_shm_surface_attach,
            test_shm_surface_attach_too_small,
            test_shm_surface_attach_overflow,
            test_shm_mapping_overflow,
        ]
    );

    define_test_suite!(
        rigorous,
        SUITE_SCHEDULER,
        [
            test_page_alloc_write_verify,
            test_page_alloc_zero_full_page,
            test_page_alloc_no_stale_data,
            test_heap_boundary_write,
            test_heap_no_overlap,
            test_heap_double_free_defensive,
            test_heap_large_block_integrity,
            test_heap_stress_cycles,
            test_page_alloc_multipage_integrity,
        ]
    );

    define_test_suite!(
        process_vm,
        SUITE_SCHEDULER,
        [
            test_process_vm_create_destroy_memory,
            test_process_vm_alloc_and_access,
            test_process_vm_brk_expansion,
            test_cow_page_isolation,
            test_cow_fault_handling,
            test_multiple_process_vms,
            test_vma_flags_retrieval,
        ]
    );

    define_test_suite!(
        sched_core,
        SUITE_SCHEDULER,
        [
            test_state_transition_ready_to_running,
            test_state_transition_running_to_blocked,
            test_state_transition_invalid_terminated_to_running,
            test_state_transition_invalid_blocked_to_running,
            test_create_max_tasks,
            test_create_over_max_tasks,
            test_rapid_create_destroy_cycle,
            test_schedule_to_empty_queue,
            test_schedule_duplicate_task,
            test_schedule_null_task,
            test_unschedule_not_in_queue,
            test_priority_ordering,
            test_idle_priority_last,
            test_timer_tick_no_current_task,
            test_timer_tick_decrements_slice,
            test_terminate_invalid_id,
            test_terminate_nonexistent_id,
            test_double_terminate,
            test_find_invalid_id,
            test_get_info_null_output,
            test_create_null_entry,
            test_create_conflicting_flags,
            test_create_null_name,
            test_scheduler_starts_disabled,
            test_schedule_while_disabled,
            test_many_same_priority_tasks,
            test_interleaved_operations,
        ]
    );

    define_test_suite!(
        demand_paging,
        SUITE_SCHEDULER,
        [
            test_demand_fault_present_page,
            test_demand_fault_no_vma,
            test_demand_fault_non_lazy_vma,
            test_demand_fault_valid_lazy_vma,
            test_demand_permission_deny_write_ro,
            test_demand_permission_deny_user_kernel,
            test_demand_permission_deny_exec,
            test_demand_permission_allow_read,
            test_demand_permission_allow_write,
            test_demand_handle_null_page_dir,
            test_demand_handle_no_vma,
            test_demand_handle_success,
            test_demand_handle_permission_denied,
            test_demand_handle_page_boundary,
            test_demand_multiple_faults,
            test_demand_double_fault,
            test_demand_invalid_process_id,
        ]
    );

    define_test_suite!(
        oom,
        SUITE_SCHEDULER,
        [
            test_page_alloc_until_oom,
            test_page_alloc_fragmentation_oom,
            test_dma_allocation_exhaustion,
            test_heap_alloc_pressure,
            test_process_vm_creation_pressure,
            test_heap_expansion_under_pressure,
            test_zero_flag_under_pressure,
            test_kzalloc_zeroed_under_pressure,
            test_alloc_free_cycles_no_leak,
            test_multiorder_alloc_failure,
            test_process_heap_expansion_oom,
            test_refcount_during_oom,
        ]
    );

    define_test_suite!(
        cow_edge,
        SUITE_SCHEDULER,
        [
            test_cow_read_not_cow_fault,
            test_cow_not_present_not_cow,
            test_cow_handle_null_pagedir,
            test_cow_handle_not_cow_page,
            test_cow_single_ref_upgrade,
            test_cow_multi_ref_copy,
            test_cow_page_boundary,
            test_cow_clone_modify_both,
            test_cow_multiple_clones,
            test_cow_no_collateral_damage,
            test_cow_handle_invalid_address,
        ]
    );

    define_test_suite!(
        syscall_valid,
        SUITE_SCHEDULER,
        slopos_core::run_syscall_validation_tests
    );
    define_test_suite!(
        exception,
        SUITE_SCHEDULER,
        crate::exception_tests::run_exception_tests
    );
    define_test_suite!(exec, SUITE_SCHEDULER, slopos_core::run_exec_tests);
    define_test_suite!(irq, SUITE_SCHEDULER, slopos_core::run_irq_tests);
    define_test_suite!(
        ioapic,
        SUITE_SCHEDULER,
        slopos_drivers::ioapic_tests::run_ioapic_tests
    );
    define_test_suite!(context, SUITE_SCHEDULER, slopos_core::run_context_tests);
    define_test_suite!(tlb, SUITE_SCHEDULER, slopos_mm::tlb_tests::run_tlb_tests);
    define_test_suite!(mmio, SUITE_SCHEDULER, slopos_mm::mmio_tests::run_mmio_tests);

    // FPU/SSE suite requires custom implementation due to inline assembly
    const FPU_NAME: &[u8] = b"fpu_sse\0";

    fn run_fpu_suite(_config: *const HarnessConfig, out: *mut TestSuiteResult) -> i32 {
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
        if let Some(out_ref) = unsafe { out.as_mut() } {
            out_ref.name = FPU_NAME.as_ptr() as *const c_char;
            out_ref.total = total;
            out_ref.passed = passed;
            out_ref.failed = total.saturating_sub(passed);
            out_ref.elapsed_ms = elapsed;
        }
        if passed == total { 0 } else { -1 }
    }

    pub static FPU_SUITE_DESC: TestSuiteDesc = TestSuiteDesc {
        name: FPU_NAME.as_ptr() as *const c_char,
        mask_bit: SUITE_SCHEDULER,
        run: Some(run_fpu_suite),
    };

    pub fn register_all() {
        register_test_suites!(
            super::tests_register_suite,
            VM_SUITE_DESC,
            HEAP_SUITE_DESC,
            EXT2_SUITE_DESC,
            PRIVSEP_SUITE_DESC,
            FPU_SUITE_DESC,
            PAGE_ALLOC_SUITE_DESC,
            HEAP_EXT_SUITE_DESC,
            PAGING_SUITE_DESC,
            RING_BUF_SUITE_DESC,
            SPINLOCK_SUITE_DESC,
            SHM_SUITE_DESC,
            RIGOROUS_SUITE_DESC,
            PROCESS_VM_SUITE_DESC,
            SCHED_CORE_SUITE_DESC,
            DEMAND_PAGING_SUITE_DESC,
            OOM_SUITE_DESC,
            COW_EDGE_SUITE_DESC,
            SYSCALL_VALID_SUITE_DESC,
            EXCEPTION_SUITE_DESC,
            EXEC_SUITE_DESC,
            IRQ_SUITE_DESC,
            IOAPIC_SUITE_DESC,
            CONTEXT_SUITE_DESC,
            TLB_SUITE_DESC,
            MMIO_SUITE_DESC,
        );
    }
}
