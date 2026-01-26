use core::ffi::c_int;

pub mod config;
pub mod harness;
mod runner;
pub mod suite_masks;

mod assertions;
pub use config::{Suite, TestConfig, Verbosity, config_from_cmdline};
pub use harness::{
    HARNESS_MAX_SUITES, HarnessConfig, TestRunSummary, TestSuiteDesc, TestSuiteResult,
    cycles_to_ms, estimate_cycles_per_ms, measure_elapsed_ms,
};
pub use runner::run_single_test;
pub use suite_masks::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestResult {
    Pass,
    Fail,
    Panic,
    Skipped,
}

impl TestResult {
    #[inline]
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }

    #[inline]
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Fail | Self::Panic)
    }

    #[inline]
    pub fn from_c_int(val: c_int) -> Self {
        if val == 0 { Self::Pass } else { Self::Fail }
    }

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

#[macro_export]
macro_rules! pass {
    () => {
        $crate::testing::TestResult::Pass
    };
}

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

#[macro_export]
macro_rules! run_test {
    ($passed:expr, $total:expr, $test_fn:expr) => {{
        $total += 1;
        let result = $crate::testing::run_single_test(stringify!($test_fn), || $test_fn().into());
        if result.is_pass() {
            $passed += 1;
        }
        result
    }};

    ($test_fn:expr) => {{ $crate::testing::run_single_test(stringify!($test_fn), || $test_fn().into()) }};

    ($name:expr, $test_fn:expr) => {{ $crate::testing::run_single_test($name, || $test_fn().into()) }};
}

#[macro_export]
macro_rules! define_test_suite {
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

#[macro_export]
macro_rules! register_test_suites {
    ($register_fn:path, $($suite_desc:expr),* $(,)?) => {
        $(
            let _ = $register_fn(&$suite_desc);
        )*
    };
}
