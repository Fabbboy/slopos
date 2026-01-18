#![no_std]

use core::ffi::{c_char, c_int};
use core::ptr;

use slopos_drivers::interrupt_test::interrupt_test_request_shutdown;
use slopos_drivers::interrupts::SUITE_SCHEDULER;
pub use slopos_drivers::interrupts::{InterruptTestConfig, Verbosity as InterruptTestVerbosity};
use slopos_lib::klog_info;

pub const TESTS_MAX_SUITES: usize = 8;
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

    use slopos_mm::tests::{
        test_heap_fragmentation_behind_head, test_heap_free_list_search,
        test_process_vm_counter_reset, test_process_vm_slot_reuse,
    };

    fn run_vm_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let mut total = 0u32;

        total += 1;
        if test_process_vm_slot_reuse() == 0 {
            passed += 1;
        }
        total += 1;
        if test_process_vm_counter_reset() == 0 {
            passed += 1;
        }

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, VM_NAME, total, passed, elapsed);
        if passed == total { 0 } else { -1 }
    }

    fn run_heap_suite(_config: *const InterruptTestConfig, out: *mut TestSuiteResult) -> i32 {
        let start = slopos_lib::tsc::rdtsc();
        let mut passed = 0u32;
        let total = 2u32;

        if test_heap_free_list_search() == 0 {
            passed += 1;
        }
        if test_heap_fragmentation_behind_head() == 0 {
            passed += 1;
        }

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
                src = in(xmm_reg) pattern2,
            );
            core::arch::asm!(
                "movdqa {dst}, xmm0",
                dst = out(xmm_reg) readback2,
            );
        }

        unsafe {
            _mm_storeu_si128(result.as_mut_ptr() as *mut __m128i, readback2);
        }
        if result == expected_bytes {
            passed += 1;
        }

        let elapsed = measure_elapsed_ms(start, slopos_lib::tsc::rdtsc());
        fill_simple_result(out, FPU_NAME, total, passed, elapsed);
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

        let _ = tests_register_suite(&VM_SUITE_DESC);
        let _ = tests_register_suite(&HEAP_SUITE_DESC);
        let _ = tests_register_suite(&EXT2_SUITE_DESC);
        let _ = tests_register_suite(&PRIVSEP_SUITE_DESC);
        let _ = tests_register_suite(&FPU_SUITE_DESC);
    }
}
