//! Test harness infrastructure for kernel integration tests.
//!
//! This module provides the low-level types and registration mechanism for the
//! kernel test harness. It is designed to work with the `define_test_suite!` macro
//! to eliminate boilerplate when defining test suites.
//!
//! # Architecture
//!
//! The test harness consists of:
//! - `TestSuiteResult`: Per-suite execution results (passed/failed/elapsed time)
//! - `TestSuiteDesc`: Static descriptor for a test suite (name, mask, runner)
//! - `TestRunSummary`: Aggregated results across all suites
//! - Registry: Global array of registered suite descriptors
//!
//! # Usage
//!
//! Use the `define_test_suite!` macro instead of manually implementing runners:
//!
//! ```ignore
//! define_test_suite!(page_alloc, SUITE_MEMORY, [
//!     test_page_alloc_single,
//!     test_page_alloc_multi,
//! ]);
//! ```

use core::ffi::{c_char, c_int};
use core::ptr;

/// Maximum number of test suites that can be registered.
pub const HARNESS_MAX_SUITES: usize = 30;

/// Default cycles per millisecond estimate (3 GHz).
const DEFAULT_CYCLES_PER_MS: u64 = 3_000_000;

// Re-export suite mask constants from a central location
pub use crate::testing::suite_masks::*;

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

/// Type alias for suite runner functions.
///
/// Uses raw pointer to opaque config to avoid circular dependency with drivers crate.
/// The actual config type is `InterruptTestConfig` from `slopos_drivers::interrupts`.
pub type SuiteRunnerFn = fn(*const (), *mut TestSuiteResult) -> i32;

/// Alias for backward compatibility - HarnessConfig is defined in drivers::interrupts
pub type HarnessConfig = ();

/// Static descriptor for a test suite.
///
/// This is created by `define_test_suite!` and registered with the harness.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TestSuiteDesc {
    pub name: *const c_char,
    pub mask_bit: u32,
    pub run: Option<SuiteRunnerFn>,
}

// SAFETY: TestSuiteDesc contains only raw pointers to static data and function pointers.
// These are inherently thread-safe for read-only access.
unsafe impl Sync for TestSuiteDesc {}

/// Aggregated results from running all test suites.
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

static mut CACHED_CYCLES_PER_MS: u64 = 0;

fn cached_cycles_per_ms_mut() -> *mut u64 {
    &raw mut CACHED_CYCLES_PER_MS
}

/// Estimate CPU cycles per millisecond using CPUID if available.
pub fn estimate_cycles_per_ms() -> u64 {
    unsafe {
        if *cached_cycles_per_ms_mut() != 0 {
            return *cached_cycles_per_ms_mut();
        }
    }

    let (max_leaf, _, _, _) = crate::cpu::cpuid(0);
    let mut cycles_per_ms = DEFAULT_CYCLES_PER_MS;
    if max_leaf >= 0x16 {
        let (freq_mhz, _, _, _) = crate::cpu::cpuid(0x16);
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
