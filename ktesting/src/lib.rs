#![no_std]
#![feature(allocator_api)]
#![feature(sync_unsafe_cell)]

pub mod capture;
pub mod config;
pub mod filter;
pub mod harness;
pub mod kernel_phase_summary;
pub mod ktap;
pub mod registry;
mod result;
mod runner;

mod assertions;
#[cfg(feature = "qemu-exit")]
pub mod qemu_signal;

#[cfg(feature = "tests")]
pub mod bootstrap_tests;
#[cfg(feature = "tests")]
pub mod exception_tests;
#[cfg(feature = "tests")]
pub mod fpu_tests;
#[cfg(feature = "tests")]
pub mod xsave_tests;
#[cfg(feature = "tests")]
pub mod zz_lockdep_tests;

pub use config::{config_from_cmdline, TestConfig, Verbosity};
pub use harness::{
    cycles_to_ms, estimate_cycles_per_ms, measure_elapsed_ms, tests_mark_panic,
    tests_request_shutdown, tests_reset_panic_state, tests_run_all, tests_run_userland,
    TestRunSummary,
};
pub use registry::{TestDesc, TestKind, FLAG_EXPECTED_PANIC};
pub use result::{TestOutcome, TestResult};
pub use runner::execute_test;
pub use slopos_service_core::paste;

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
        slopos_ostd::klog_info!("TEST FAIL: {}", $msg);
        $crate::TestResult::Fail
    }};
    ($fmt:expr, $($arg:tt)*) => {{
        slopos_ostd::klog_info!(concat!("TEST FAIL: ", $fmt), $($arg)*);
        $crate::TestResult::Fail
    }};
}

/// Register a single test function as a `TestDesc` in `.test_registry`.
///
/// The function must have signature `fn() -> TestResult`. The harness
/// runs each entry under `catch_panic!`, capturing klog output for the
/// duration of the test.
///
/// ```ignore
/// fn my_test() -> TestResult { TestResult::Pass }
/// slopos_testing::stest!(name = my_test);
/// ```
#[macro_export]
macro_rules! stest {
    (name = $ident:ident) => {
        $crate::stest!(name = $ident, flags = 0);
    };

    (name = $ident:ident, flags = $flags:expr) => {
        $crate::paste::paste! {
            fn [<__stest_thunk_ $ident>]() -> $crate::TestResult {
                $crate::execute_test($ident as fn() -> $crate::TestResult)
            }

            slopos_ostd::registry_entry! {
            tests,
            #[allow(non_upper_case_globals)]
            pub static [<TEST_DESC_ $ident>]: $crate::TestDesc = $crate::TestDesc {
                name: stringify!($ident),
                module: module_path!(),
                file: file!(),
                line: line!(),
                run: [<__stest_thunk_ $ident>],
                kind: $crate::TestKind::Kernel,
                flags: $flags,
                bin: None,
                argv: &[],
            };
            }
        }
    };

    (name = $ident:ident, suite = $suite:ident) => {
        $crate::stest!(name = $ident, suite = $suite, flags = 0);
    };

    (name = $ident:ident, suite = $suite:ident, flags = $flags:expr) => {
        $crate::paste::paste! {
            fn [<__stest_thunk_ $suite _ $ident>]() -> $crate::TestResult {
                $crate::execute_test($ident as fn() -> $crate::TestResult)
            }

            slopos_ostd::registry_entry! {
            tests,
            #[allow(non_upper_case_globals)]
            pub static [<TEST_DESC_ $suite _ $ident>]: $crate::TestDesc = $crate::TestDesc {
                name: stringify!($ident),
                module: module_path!(),
                file: file!(),
                line: line!(),
                run: [<__stest_thunk_ $suite _ $ident>],
                kind: $crate::TestKind::Kernel,
                flags: $flags,
                bin: None,
                argv: &[],
            };
            }
        }
    };
}
