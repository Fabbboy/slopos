#![no_std]
#![feature(sync_unsafe_cell)]

use core::ffi::c_int;

pub mod config;
pub mod harness;
mod runner;

mod assertions;
pub use config::{config_from_cmdline, TestConfig, Verbosity};
pub use harness::{
    cycles_to_ms, estimate_cycles_per_ms, measure_elapsed_ms, TestRunSummary, TestSuiteDesc,
    TestSuiteResult, HARNESS_MAX_SUITES,
};
pub use runner::run_single_test;
pub use slopos_service_core::paste;

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
        matches!(self, Self::Pass | Self::Skipped)
    }

    #[inline]
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Fail | Self::Panic)
    }

    #[inline]
    pub fn to_c_int(self) -> c_int {
        match self {
            Self::Pass | Self::Skipped => 0,
            Self::Fail | Self::Panic => -1,
        }
    }
}

#[macro_export]
macro_rules! pass {
    () => {
        $crate::TestResult::Pass
    };
}

#[macro_export]
macro_rules! fail {
    () => {
        $crate::TestResult::Fail
    };
    ($msg:expr) => {{
        slopos_utils::klog_info!("TEST FAIL: {}", $msg);
        $crate::TestResult::Fail
    }};
    ($fmt:expr, $($arg:tt)*) => {{
        slopos_utils::klog_info!(concat!("TEST FAIL: ", $fmt), $($arg)*);
        $crate::TestResult::Fail
    }};
}

#[macro_export]
macro_rules! run_test {
    ($passed:expr, $total:expr, $test_fn:expr) => {{
        $total += 1;
        let result = $crate::run_single_test(stringify!($test_fn), || $test_fn());
        if result.is_pass() {
            $passed += 1;
        }
        result
    }};

    ($test_fn:expr) => {{
        $crate::run_single_test(stringify!($test_fn), || $test_fn())
    }};

    ($name:expr, $test_fn:expr) => {{
        $crate::run_single_test($name, || $test_fn())
    }};
}

#[macro_export]
macro_rules! define_test_suite {
    ($suite_name:ident, [$($test_fn:path),* $(,)?]) => {
        $crate::paste::paste! {
            const [<$suite_name:upper _NAME>]: &[u8] = concat!(stringify!($suite_name), "\0").as_bytes();

            fn [<run_ $suite_name _suite>](
                _config: *const (),
                out: *mut $crate::TestSuiteResult,
            ) -> i32 {
                let start = slopos_arch::tsc::rdtsc();
                let mut passed = 0u32;
                let mut total = 0u32;

                $(
                    $crate::run_test!(passed, total, $test_fn);
                )*

                let elapsed = $crate::measure_elapsed_ms(start, slopos_arch::tsc::rdtsc());

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

            #[used]
            #[unsafe(link_section = ".test_registry")]
            pub static [<$suite_name:upper _SUITE_DESC>]: $crate::TestSuiteDesc = $crate::TestSuiteDesc {
                name: [<$suite_name:upper _NAME>].as_ptr() as *const core::ffi::c_char,
                run: Some([<run_ $suite_name _suite>]),
            };
        }
    };

    ($suite_name:ident, $runner_fn:path, single) => {
        $crate::paste::paste! {
            const [<$suite_name:upper _NAME>]: &[u8] = concat!(stringify!($suite_name), "\0").as_bytes();

            fn [<run_ $suite_name _suite>](
                _config: *const (),
                out: *mut $crate::TestSuiteResult,
            ) -> i32 {
                let start = slopos_arch::tsc::rdtsc();
                let result = slopos_utils::catch_panic!({ $runner_fn().to_c_int() });
                let passed = if result == 0 { 1u32 } else { 0u32 };
                let elapsed = $crate::measure_elapsed_ms(start, slopos_arch::tsc::rdtsc());

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

            #[used]
            #[unsafe(link_section = ".test_registry")]
            pub static [<$suite_name:upper _SUITE_DESC>]: $crate::TestSuiteDesc = $crate::TestSuiteDesc {
                name: [<$suite_name:upper _NAME>].as_ptr() as *const core::ffi::c_char,
                run: Some([<run_ $suite_name _suite>]),
            };
        }
    };
}
