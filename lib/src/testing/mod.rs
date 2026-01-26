use core::ffi::c_int;

pub mod config;
mod fixture;
pub mod harness;
mod runner;
pub mod suite_masks;

pub use config::{config_from_cmdline, Suite, TestConfig, Verbosity};
pub use fixture::{FixtureKind, NoFixture, TestFixture};
pub use harness::{
    cycles_to_ms, estimate_cycles_per_ms, measure_elapsed_ms, HarnessConfig, TestRunSummary,
    TestSuiteDesc, TestSuiteResult, HARNESS_MAX_SUITES,
};
pub use runner::run_single_test;
pub use suite_masks::*;

/// Result of a single test execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestResult {
    /// Test passed successfully
    Pass,
    /// Test failed (assertion or explicit failure)
    Fail,
    /// Test panicked unexpectedly
    Panic,
    /// Test was skipped (e.g., fixture setup failed)
    Skipped,
}

impl TestResult {
    /// Returns true if the test passed.
    #[inline]
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }

    /// Returns true if the test failed or panicked.
    #[inline]
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Fail | Self::Panic)
    }

    /// Convert from C-style return code (0 = pass, non-zero = fail).
    #[inline]
    pub fn from_c_int(val: c_int) -> Self {
        if val == 0 {
            Self::Pass
        } else {
            Self::Fail
        }
    }

    /// Convert to C-style return code (0 = pass, -1 = fail).
    #[inline]
    pub fn to_c_int(self) -> c_int {
        match self {
            Self::Pass | Self::Skipped => 0,
            Self::Fail | Self::Panic => -1,
        }
    }
}

impl From<i32> for TestResult {
    fn from(val: i32) -> Self {
        Self::from_c_int(val as c_int)
    }
}

impl From<TestResult> for c_int {
    fn from(val: TestResult) -> Self {
        val.to_c_int()
    }
}

/// Return a passing test result.
///
/// # Example
/// ```ignore
/// if condition_met {
///     return pass!();
/// }
/// ```
#[macro_export]
macro_rules! pass {
    () => {
        $crate::testing::TestResult::Pass
    };
}

/// Return a failing test result with optional message.
///
/// # Example
/// ```ignore
/// if !condition {
///     return fail!("Expected condition to be true");
/// }
/// ```
#[macro_export]
macro_rules! fail {
    () => {
        $crate::testing::TestResult::Fail
    };
    ($msg:expr) => {{
        $crate::klog_info!("TEST FAIL: {}\n", $msg);
        $crate::testing::TestResult::Fail
    }};
    ($fmt:expr, $($arg:tt)*) => {{
        $crate::klog_info!(concat!("TEST FAIL: ", $fmt, "\n"), $($arg)*);
        $crate::testing::TestResult::Fail
    }};
}

/// Run a single test with optional fixture and panic catching.
///
/// # Usage variants
///
/// ```ignore
/// // Basic: just function name (no fixture)
/// run_test!(test_function)
///
/// // With custom name (no fixture)
/// run_test!("custom name", test_function)
///
/// // With fixture type
/// run_test!(test_function, SchedFixture)
///
/// // With custom name and fixture
/// run_test!("custom name", test_function, SchedFixture)
///
/// // Accumulating results (for suite runners)
/// run_test!(passed, total, test_function)
/// run_test!(passed, total, test_function, SchedFixture)
/// ```
#[macro_export]
macro_rules! run_test {
    // Accumulating variant: (passed, total, test_fn)
    ($passed:expr, $total:expr, $test_fn:expr) => {{
        $total += 1;
        let result = $crate::testing::run_single_test(
            stringify!($test_fn),
            || $test_fn().into(),
            $crate::testing::FixtureKind::None,
        );
        if result.is_pass() {
            $passed += 1;
        }
        result
    }};

    // Accumulating variant with fixture: (passed, total, test_fn, Fixture)
    ($passed:expr, $total:expr, $test_fn:expr, $fixture:ty) => {{
        $total += 1;
        let result = $crate::testing::run_single_test(
            stringify!($test_fn),
            || $test_fn().into(),
            <$fixture as $crate::testing::TestFixture>::KIND,
        );
        if result.is_pass() {
            $passed += 1;
        }
        result
    }};

    // Simple variant: just function
    ($test_fn:expr) => {{
        $crate::testing::run_single_test(
            stringify!($test_fn),
            || $test_fn().into(),
            $crate::testing::FixtureKind::None,
        )
    }};

    // With fixture type
    ($test_fn:expr, $fixture:ty) => {{
        $crate::testing::run_single_test(
            stringify!($test_fn),
            || $test_fn().into(),
            <$fixture as $crate::testing::TestFixture>::KIND,
        )
    }};

    // Custom name, no fixture
    ($name:expr, $test_fn:expr) => {{
        $crate::testing::run_single_test(
            $name,
            || $test_fn().into(),
            $crate::testing::FixtureKind::None,
        )
    }};

    // Custom name with fixture
    ($name:expr, $test_fn:expr, $fixture:ty) => {{
        $crate::testing::run_single_test(
            $name,
            || $test_fn().into(),
            <$fixture as $crate::testing::TestFixture>::KIND,
        )
    }};
}

/// Define a test suite for the kernel test harness with automatic registration.
///
/// Generates:
/// - A runner function compatible with `TestSuiteDesc`
/// - A static `TestSuiteDesc` for registration
///
/// # Variants
///
/// 1. **Inline tests**: List individual test functions (preferred)
/// ```ignore
/// define_test_suite!(page_alloc, SUITE_MEMORY, [
///     test_page_alloc_single,
///     test_page_alloc_multi,
/// ]);
/// ```
///
/// 2. **Single test**: Wrap a single `fn() -> c_int` function
/// ```ignore
/// define_test_suite!(privsep, SUITE_SCHEDULER, run_privilege_test, single);
/// ```
#[macro_export]
macro_rules! define_test_suite {
    // Variant 1: Inline test list
    ($suite_name:ident, $mask:expr, [$($test_fn:path),* $(,)?]) => {
        $crate::paste::paste! {
            const [<$suite_name:upper _NAME>]: &[u8] = concat!(stringify!($suite_name), "\0").as_bytes();

            fn [<run_ $suite_name _suite>](
                _config: *const $crate::testing::HarnessConfig,
                out: *mut $crate::testing::TestSuiteResult,
            ) -> i32 {
                let start = $crate::tsc::rdtsc();
                let mut passed = 0u32;
                let mut total = 0u32;

                $(
                    $crate::run_test!(passed, total, $test_fn);
                )*

                let elapsed = $crate::testing::measure_elapsed_ms(start, $crate::tsc::rdtsc());

                if let Some(out_ref) = unsafe { out.as_mut() } {
                    out_ref.name = [<$suite_name:upper _NAME>].as_ptr() as *const core::ffi::c_char;
                    out_ref.total = total;
                    out_ref.passed = passed;
                    out_ref.failed = total.saturating_sub(passed);
                    out_ref.exceptions_caught = 0;
                    out_ref.unexpected_exceptions = 0;
                    out_ref.elapsed_ms = elapsed;
                    out_ref.timed_out = 0;
                }

                if passed == total { 0 } else { -1 }
            }

            pub static [<$suite_name:upper _SUITE_DESC>]: $crate::testing::TestSuiteDesc = $crate::testing::TestSuiteDesc {
                name: [<$suite_name:upper _NAME>].as_ptr() as *const core::ffi::c_char,
                mask_bit: $mask,
                run: Some([<run_ $suite_name _suite>]),
            };
        }
    };

    // Variant 2: Single test function returning c_int (with panic catching)
    ($suite_name:ident, $mask:expr, $runner_fn:path, single) => {
        $crate::paste::paste! {
            const [<$suite_name:upper _NAME>]: &[u8] = concat!(stringify!($suite_name), "\0").as_bytes();

            fn [<run_ $suite_name _suite>](
                _config: *const $crate::testing::HarnessConfig,
                out: *mut $crate::testing::TestSuiteResult,
            ) -> i32 {
                let start = $crate::tsc::rdtsc();
                let result = $crate::catch_panic!({ $runner_fn() });
                let passed = if result == 0 { 1u32 } else { 0u32 };
                let elapsed = $crate::testing::measure_elapsed_ms(start, $crate::tsc::rdtsc());

                if let Some(out_ref) = unsafe { out.as_mut() } {
                    out_ref.name = [<$suite_name:upper _NAME>].as_ptr() as *const core::ffi::c_char;
                    out_ref.total = 1;
                    out_ref.passed = passed;
                    out_ref.failed = 1 - passed;
                    out_ref.exceptions_caught = 0;
                    out_ref.unexpected_exceptions = 0;
                    out_ref.elapsed_ms = elapsed;
                    out_ref.timed_out = 0;
                }

                if result == 0 { 0 } else { -1 }
            }

            pub static [<$suite_name:upper _SUITE_DESC>]: $crate::testing::TestSuiteDesc = $crate::testing::TestSuiteDesc {
                name: [<$suite_name:upper _NAME>].as_ptr() as *const core::ffi::c_char,
                mask_bit: $mask,
                run: Some([<run_ $suite_name _suite>]),
            };
        }
    };
}

/// Register multiple test suites with the harness in one call.
#[macro_export]
macro_rules! register_test_suites {
    ($register_fn:path, $($suite_desc:expr),* $(,)?) => {
        $(
            let _ = $register_fn(&$suite_desc);
        )*
    };
}
