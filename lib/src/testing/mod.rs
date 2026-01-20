//! Unified test framework for SlopOS kernel tests.
//!
//! This module provides a consistent, DRY approach to writing kernel tests:
//! - Single `TestResult` type for all test outcomes
//! - RAII-based test fixtures for automatic setup/teardown
//! - Unified `run_test!` macro with panic catching
//! - `test_suite!` macro for declarative suite definition
//!
//! # Example
//!
//! ```ignore
//! use slopos_lib::testing::{TestResult, pass, fail};
//!
//! pub fn test_something() -> TestResult {
//!     if some_condition {
//!         return fail!("Condition not met");
//!     }
//!     pass!()
//! }
//! ```

use core::ffi::c_int;

mod fixture;
mod runner;

pub use fixture::{FixtureKind, NoFixture, TestFixture};
pub use runner::{SuiteResults, run_single_test, run_suite};

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
        if val == 0 { Self::Pass } else { Self::Fail }
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

/// Metadata for a single test case.
pub struct TestCase {
    /// Name of the test (usually function name)
    pub name: &'static str,
    /// Test function returning TestResult
    pub func: fn() -> TestResult,
    /// Fixture type to use for setup/teardown
    pub fixture: FixtureKind,
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

/// Declare a test suite with automatic result collection.
///
/// # Example
///
/// ```ignore
/// test_suite!(page_alloc_suite, [
///     test_page_alloc_single,
///     test_page_alloc_multi_order,
///     test_page_alloc_free_cycle,
/// ]);
///
/// // With fixture for all tests
/// test_suite!(scheduler_suite, SchedFixture, [
///     test_create_task,
///     test_schedule_task,
/// ]);
/// ```
#[macro_export]
macro_rules! test_suite {
    // No fixture variant
    ($name:ident, [$($test:ident),* $(,)?]) => {
        pub fn $name() -> $crate::testing::SuiteResults {
            let tests: &[$crate::testing::TestCase] = &[
                $(
                    $crate::testing::TestCase {
                        name: stringify!($test),
                        func: || $test().into(),
                        fixture: $crate::testing::FixtureKind::None,
                    },
                )*
            ];
            $crate::testing::run_suite(stringify!($name), tests)
        }
    };

    // With fixture variant
    ($name:ident, $fixture:ty, [$($test:ident),* $(,)?]) => {
        pub fn $name() -> $crate::testing::SuiteResults {
            let tests: &[$crate::testing::TestCase] = &[
                $(
                    $crate::testing::TestCase {
                        name: stringify!($test),
                        func: || $test().into(),
                        fixture: <$fixture as $crate::testing::TestFixture>::KIND,
                    },
                )*
            ];
            $crate::testing::run_suite(stringify!($name), tests)
        }
    };
}
