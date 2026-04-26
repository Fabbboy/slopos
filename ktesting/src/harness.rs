// Test harness types: TestSuiteResult, TestSuiteDesc, TestRunSummary.
// Suites are auto-registered via #[link_section = ".test_registry"] in define_test_suite!.

use core::cell::SyncUnsafeCell;
use core::ffi::{c_char, c_int};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_sync::StateFlag;
use slopos_utils::klog_info;

use crate::config::TestConfig;

/// Maximum number of test suites that can be registered.
pub const HARNESS_MAX_SUITES: usize = 64;

/// Default cycles per millisecond estimate (3 GHz).
const DEFAULT_CYCLES_PER_MS: u64 = 3_000_000;

/// Result of executing a single test suite.
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

impl TestSuiteResult {
    /// Create a new result with just the suite name set.
    pub const fn new(name: *const c_char) -> Self {
        Self {
            name,
            total: 0,
            passed: 0,
            failed: 0,
            exceptions_caught: 0,
            unexpected_exceptions: 0,
            elapsed_ms: 0,
            timed_out: 0,
        }
    }

    /// Fill in results from a (passed, total) tuple and elapsed time.
    pub fn fill(&mut self, passed: u32, total: u32, elapsed_ms: u32) {
        self.total = total;
        self.passed = passed;
        self.failed = total.saturating_sub(passed);
        self.elapsed_ms = elapsed_ms;
    }

    /// Check if all tests in this suite passed.
    pub fn all_passed(&self) -> bool {
        self.failed == 0 && self.unexpected_exceptions == 0 && self.timed_out == 0
    }
}

pub type SuiteRunnerFn = fn(*const (), *mut TestSuiteResult) -> i32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TestSuiteDesc {
    pub name: *const c_char,
    pub run: Option<SuiteRunnerFn>,
}

// SAFETY: TestSuiteDesc contains only raw pointers to static data and function pointers.
// These are inherently thread-safe for read-only access.
unsafe impl Sync for TestSuiteDesc {}

/// Aggregated results from running all test suites.
///
/// SAFETY: All fields are integer/byte-array primitives; the all-zero
/// bit pattern is a valid value, allowing heap-direct allocation via
/// `KBox::<TestRunSummary>::zeroed()` to avoid materialising the
/// ~2.6 KiB `Default::default()` rvalue on a caller's stack.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TestRunSummary {
    pub suites: [TestSuiteResult; HARNESS_MAX_SUITES],
    pub suite_count: usize,
    pub total_tests: u32,
    pub passed: u32,
    pub failed: u32,
    pub exceptions_caught: u32,
    pub unexpected_exceptions: u32,
    pub elapsed_ms: u32,
    pub timed_out: c_int,
}

// SAFETY: see struct doc; all fields zero-valid.
unsafe impl slopos_alloc::Zeroable for TestRunSummary {}

impl Default for TestRunSummary {
    fn default() -> Self {
        Self {
            suites: [TestSuiteResult::default(); HARNESS_MAX_SUITES],
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

impl TestRunSummary {
    /// Add results from a single suite to the summary.
    pub fn add_suite_result(&mut self, result: &TestSuiteResult) {
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

    /// Check if all tests across all suites passed.
    pub fn all_passed(&self) -> bool {
        self.failed == 0 && self.unexpected_exceptions == 0 && self.timed_out == 0
    }
}

// =============================================================================
// Time measurement utilities
// =============================================================================

static CACHED_CYCLES_PER_MS: SyncUnsafeCell<u64> = SyncUnsafeCell::new(0);

fn cached_cycles_per_ms_mut() -> *mut u64 {
    CACHED_CYCLES_PER_MS.get()
}

/// Estimate CPU cycles per millisecond using CPUID if available.
pub fn estimate_cycles_per_ms() -> u64 {
    unsafe {
        if *cached_cycles_per_ms_mut() != 0 {
            return *cached_cycles_per_ms_mut();
        }
    }

    let (max_leaf, _, _, _) = slopos_arch::cpu::cpuid(0);
    let mut cycles_per_ms = DEFAULT_CYCLES_PER_MS;
    if max_leaf >= 0x16 {
        let (freq_mhz, _, _, _) = slopos_arch::cpu::cpuid(0x16);
        if freq_mhz != 0 {
            cycles_per_ms = freq_mhz as u64 * 1_000;
        }
    }

    unsafe {
        *cached_cycles_per_ms_mut() = cycles_per_ms;
    }
    cycles_per_ms
}

/// Convert TSC cycles to milliseconds.
pub fn cycles_to_ms(cycles: u64) -> u32 {
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

/// Measure elapsed time in milliseconds between two TSC readings.
#[inline]
pub fn measure_elapsed_ms(start: u64, end: u64) -> u32 {
    cycles_to_ms(end.wrapping_sub(start))
}

// =============================================================================
// Harness driver: panic glue + tests_run_all
// =============================================================================

pub const TESTS_MAX_SUITES: usize = HARNESS_MAX_SUITES;

static PANIC_SEEN: StateFlag = StateFlag::new();
static PANIC_REPORTED: AtomicBool = AtomicBool::new(false);

pub fn tests_reset_panic_state() {
    PANIC_SEEN.set_inactive();
    PANIC_REPORTED.store(false, Ordering::Relaxed);
}

#[cfg(not(feature = "tests"))]
pub fn tests_run_all(
    _config: *const TestConfig,
    _summary: *mut TestRunSummary,
    _registry_start: *const TestSuiteDesc,
    _registry_end: *const TestSuiteDesc,
) -> i32 {
    // No-op in production: `TestRunSummary` is 6 KiB on the kernel
    // stack, tripping the stack-size gate. The real body compiles
    // under the `tests` feature (which lifts the gate via
    // scripts/build_kernel.sh).
    0
}

#[cfg(feature = "tests")]
pub fn tests_run_all(
    config: *const TestConfig,
    summary: *mut TestRunSummary,
    registry_start: *const TestSuiteDesc,
    registry_end: *const TestSuiteDesc,
) -> i32 {
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
        klog_info!("TESTS: Harness disabled");
        return 0;
    }

    klog_info!("TESTS: Starting test suites");

    let start_cycles = slopos_arch::tsc::rdtsc();
    let mut idx = 0usize;
    let mut cursor = registry_start;
    while cursor < registry_end {
        if PANIC_SEEN.is_active() {
            summary.unexpected_exceptions = summary.unexpected_exceptions.saturating_add(1);
            summary.failed = summary.failed.saturating_add(1);
            if !PANIC_REPORTED.swap(true, Ordering::Relaxed) {
                klog_info!("TESTS: panic flagged, stopping suite execution");
            }
            break;
        }

        let desc = unsafe { &*cursor };

        let suite_start = slopos_arch::tsc::rdtsc();
        let mut res = TestSuiteResult::default();
        res.name = desc.name;

        if let Some(run) = desc.run {
            let config_ptr = config as *const ();
            // Call the suite runner directly — no outer catch_panic!.
            // Individual tests use catch_panic! internally (run_single_test)
            // to catch intentional panics. Unexpected suite-level panics
            // propagate to the default panic handler which exits QEMU with
            // a failure code in test mode, avoiding deadlocks from longjmp
            // through held locks or corrupted state.
            run(config_ptr, &mut res);
        }

        if PANIC_SEEN.is_active() {
            res.unexpected_exceptions = res.unexpected_exceptions.saturating_add(1);
            res.failed = res.failed.saturating_add(1);
        }

        if cfg.timeout_ms != 0 {
            let elapsed = measure_elapsed_ms(suite_start, slopos_arch::tsc::rdtsc());
            if elapsed > cfg.timeout_ms {
                res.timed_out = 1;
                res.failed = res.failed.saturating_add(1);
                if !PANIC_REPORTED.swap(true, Ordering::Relaxed) {
                    klog_info!("TESTS: suite timeout exceeded");
                }
            }
        }

        if summary.suite_count < TESTS_MAX_SUITES {
            summary.suites[summary.suite_count] = res;
            summary.suite_count += 1;
        }

        klog_info!(
            "SUITE{} total={} pass={} fail={} elapsed={}ms",
            idx as u32,
            res.total,
            res.passed,
            res.failed,
            res.elapsed_ms,
        );
        summary.add_suite_result(&res);

        idx += 1;
        cursor = unsafe { cursor.add(1) };
    }
    let end_cycles = slopos_arch::tsc::rdtsc();
    let overall_ms = measure_elapsed_ms(start_cycles, end_cycles);
    if overall_ms > summary.elapsed_ms {
        summary.elapsed_ms = overall_ms;
    }

    klog_info!(
        "TESTS SUMMARY: total={} passed={} failed={} elapsed_ms={}",
        summary.total_tests,
        summary.passed,
        summary.failed,
        summary.elapsed_ms,
    );

    if summary.failed == 0 {
        0
    } else {
        -1
    }
}

#[cfg(feature = "qemu-exit")]
pub fn tests_request_shutdown(failed: i32) {
    crate::qemu_signal::qemu_signal_exit(failed);
}

#[cfg(not(feature = "qemu-exit"))]
pub fn tests_request_shutdown(_failed: i32) {
    // No-op when qemu-exit feature is disabled (production builds).
}

pub fn tests_mark_panic() {
    PANIC_SEEN.set_active();
    if !PANIC_REPORTED.swap(true, Ordering::Relaxed) {
        klog_info!("TESTS: panic observed");
    }
}
