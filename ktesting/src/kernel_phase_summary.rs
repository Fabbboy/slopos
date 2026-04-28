//! Cross-phase summary stash for the test harness.
//!
//! The kernel-test phase runs from the boot init pipeline (BSP bootstrap
//! stub context) and produces a `TestRunSummary`. The userland-test phase
//! runs later from `/sbin/init`'s syscall context (`SYSCALL_RUN_USERLAND_TESTS`)
//! and needs the kernel-phase totals to roll up the cumulative result and
//! decide shutdown semantics.
//!
//! These two phases live in different crates (`slopos-boot` writes; the
//! syscall handler in `slopos-core` reads), so the stash lives in the
//! shared `slopos-testing` crate.

use core::cell::SyncUnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use crate::TestRunSummary;

static KERNEL_SUMMARY: SyncUnsafeCell<TestRunSummary> = SyncUnsafeCell::new(TestRunSummary {
    total: 0,
    passed: 0,
    failed: 0,
    skipped: 0,
    over_time: 0,
    panics: 0,
    elapsed_ms: 0,
});

static KERNEL_RC: AtomicI32 = AtomicI32::new(0);

/// Whether `tests.shutdown=on` was set on the boot command line. Read by
/// the userland-phase syscall handler to decide whether to signal QEMU exit.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Whether `tests=on` was set on the boot command line. Lets the syscall
/// handler short-circuit when invoked from a non-test boot.
static TESTS_ENABLED: AtomicBool = AtomicBool::new(false);

/// Stash the kernel-phase summary + run-rc + relevant config bits for the
/// userland phase to read.
///
/// Called once from the boot init pipeline after `tests_run_all` returns.
/// Subsequent calls overwrite (intended for re-init scenarios in tests).
pub fn store_kernel_phase(summary: &TestRunSummary, rc: i32, enabled: bool, shutdown: bool) {
    // SAFETY: writers are single-threaded (the BSP boot init pipeline runs
    // sequentially); there is no concurrent reader at this point because
    // the userland phase syscall hasn't fired yet.
    unsafe {
        *KERNEL_SUMMARY.get() = *summary;
    }
    KERNEL_RC.store(rc, Ordering::Release);
    TESTS_ENABLED.store(enabled, Ordering::Release);
    SHUTDOWN_REQUESTED.store(shutdown, Ordering::Release);
}

pub fn load_kernel_phase() -> (TestRunSummary, i32) {
    // SAFETY: writes are quiescent by the time any userland-phase reader
    // executes (boot init has finished and the syscall path is the only
    // reader); a stale-but-self-consistent snapshot is acceptable.
    let summary = unsafe { *KERNEL_SUMMARY.get() };
    let rc = KERNEL_RC.load(Ordering::Acquire);
    (summary, rc)
}

pub fn tests_enabled() -> bool {
    TESTS_ENABLED.load(Ordering::Acquire)
}

pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Acquire)
}
