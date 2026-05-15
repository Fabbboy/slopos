//! Userland-side bridge to the kernel test harness.
//!
//! Test binaries call [`run`] with a slice of `(name, fn() -> bool)` pairs.
//! Each test reports its result via `SYSCALL_TEST_REPORT`, then the binary
//! exits with the failure count (capped at 255). The kernel-side runner
//! drains the structured reports and emits one indented KTAP subtest line
//! per case under the parent utest line.

use crate::pal::{Pal, Sys};
use crate::process;

/// Wire-format status passed to `SYSCALL_TEST_REPORT`. Values must match
/// [`slopos_abi::syscall::TestReportStatus`].
#[derive(Clone, Copy, Debug)]
#[repr(u32)]
pub enum TestStatus {
    Pass = 0,
    Fail = 1,
    Skip = 2,
}

/// Submit a single test result to the kernel. Best-effort: kernel-side
/// failure to record (no harness running, ring exhausted, bad pointer)
/// is swallowed — callers continue regardless.
pub fn report(status: TestStatus, name: &str, msg: &str) {
    let _ = Sys::test_report(status as u32, name.as_bytes(), msg.as_bytes());
}

/// Iterate `cases`, run each, and report its result. Exits the process
/// after the final case with `failed.min(255)` as the exit code.
///
/// The kernel utest runner uses the structured reports for fine-grained
/// roll-up; the exit code is a coarse signal that only matters when the
/// binary crashes before reporting any cases.
pub fn run(cases: &[(&'static str, fn() -> bool)]) -> ! {
    let mut failed: u32 = 0;
    for (name, f) in cases {
        let ok = f();
        let status = if ok {
            TestStatus::Pass
        } else {
            TestStatus::Fail
        };
        report(status, name, "");
        if !ok {
            failed = failed.saturating_add(1);
        }
    }
    let exit_code = failed.min(255) as i32;
    process::shim::exit(exit_code)
}
