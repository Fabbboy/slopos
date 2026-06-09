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

/// Iterate `cases`, run each, and report its result. Exits the process
/// after the final case with `failed.min(255)` as the exit code.
///
/// The kernel utest runner uses the structured reports for fine-grained
/// roll-up; the exit code is a coarse signal that only matters when the
/// binary crashes before reporting any cases.
pub fn run(cases: &[(&'static str, fn() -> bool)]) -> ! {
    run_impl(None, cases)
}

/// Like [`run`], but also prints a best-effort start/end line to stderr for
/// each case. This is intentionally separate from the structured report stream:
/// reports remain final verdicts only, while progress lines identify the active
/// case if a userland binary hangs or panics before it can report.
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
