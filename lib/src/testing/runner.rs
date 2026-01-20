//! Test execution and result collection.

use super::{FixtureKind, TestCase, TestResult};
use crate::klog_info;
use crate::tsc::rdtsc;

const TESTS_MAX_CYCLES_PER_MS: u64 = 3_000_000;

static mut CACHED_CYCLES_PER_MS: u64 = 0;

fn cached_cycles_per_ms_mut() -> *mut u64 {
    &raw mut CACHED_CYCLES_PER_MS
}

fn estimate_cycles_per_ms() -> u64 {
    unsafe {
        if *cached_cycles_per_ms_mut() != 0 {
            return *cached_cycles_per_ms_mut();
        }
    }

    let (max_leaf, _, _, _) = crate::cpu::cpuid(0);
    let mut cycles_per_ms = TESTS_MAX_CYCLES_PER_MS;
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

#[derive(Clone, Copy, Debug, Default)]
pub struct SuiteResults {
    pub name: &'static str,
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub panicked: u32,
    pub skipped: u32,
    pub elapsed_ms: u32,
}

impl SuiteResults {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            ..Default::default()
        }
    }

    pub fn all_passed(&self) -> bool {
        self.failed == 0 && self.panicked == 0
    }

    pub fn to_tuple(&self) -> (u32, u32) {
        (self.passed, self.total)
    }
}

pub fn run_single_test(
    _name: &str,
    test_fn: fn() -> TestResult,
    _fixture: FixtureKind,
) -> TestResult {
    let result = crate::catch_panic!({ test_fn().to_c_int() });

    if result == 0 {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

pub fn run_suite(name: &'static str, tests: &[TestCase]) -> SuiteResults {
    let start = rdtsc();
    let mut results = SuiteResults::new(name);
    results.total = tests.len() as u32;

    for test in tests {
        let result = run_single_test(test.name, test.func, test.fixture);
        match result {
            TestResult::Pass => results.passed += 1,
            TestResult::Fail => results.failed += 1,
            TestResult::Panic => results.panicked += 1,
            TestResult::Skipped => results.skipped += 1,
        }
    }

    results.elapsed_ms = cycles_to_ms(rdtsc().wrapping_sub(start));

    klog_info!(
        "SUITE {}: {}/{} passed ({}ms)\n",
        name,
        results.passed,
        results.total,
        results.elapsed_ms
    );

    results
}
