//! Userland-side bridge to the kernel test harness.
//!
//! Test binaries call [`run`] with `(name, fn() -> bool)` pairs; each result
//! goes to the kernel via `SYSCALL_TEST_REPORT` and becomes a KTAP subtest
//! line.

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

/// Best-effort: a kernel-side failure to record is swallowed and the caller
/// continues.
pub fn report(status: TestStatus, name: &str, msg: &str) {
    let _ = Sys::test_report(status as u32, name.as_bytes(), msg.as_bytes());
}

fn progress_write(bytes: &[u8]) {
    let _ = Sys::write(2, bytes.as_ptr(), bytes.len());
}

fn progress(prefix: &str, name: &str, phase: &str) {
    progress_write(b"utest-progress: ");
    progress_write(prefix.as_bytes());
    progress_write(b"::");
    progress_write(name.as_bytes());
    progress_write(b" ");
    progress_write(phase.as_bytes());
    progress_write(b"\n");
}

/// Runs every case, reports each result, then exits with `failed.min(255)`.
/// The exit code is a coarse fallback for a binary that crashes before
/// reporting; the structured reports are what the kernel runner rolls up.
pub fn run(cases: &[(&'static str, fn() -> bool)]) -> ! {
    run_impl(None, cases)
}

/// Like [`run`], but also prints a best-effort start/end line to stderr so the
/// active case is identifiable if the binary hangs or panics before reporting.
pub fn run_with_progress(prefix: &'static str, cases: &[(&'static str, fn() -> bool)]) -> ! {
    run_impl(Some(prefix), cases)
}

fn run_impl(progress_prefix: Option<&'static str>, cases: &[(&'static str, fn() -> bool)]) -> ! {
    let mut failed: u32 = 0;
    for (name, f) in cases {
        if let Some(prefix) = progress_prefix {
            progress(prefix, name, "start");
        }
        let ok = f();
        let status = if ok {
            TestStatus::Pass
        } else {
            TestStatus::Fail
        };
        if let Some(prefix) = progress_prefix {
            progress(prefix, name, if ok { "pass" } else { "fail" });
        }
        report(status, name, "");
        if !ok {
            failed = failed.saturating_add(1);
        }
    }
    let exit_code = failed.min(255) as i32;
    process::shim::exit(exit_code)
}
