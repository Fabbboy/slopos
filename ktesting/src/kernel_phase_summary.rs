//! Cross-phase summary stash for the test harness.
//!
//! The kernel phase runs from the boot init pipeline and the userland phase
//! later from `/sbin/init`'s syscall context, in different crates
//! (`slopos-boot` writes, `slopos-core` reads), so the totals live here.

use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use slopos_ostd::KVec;
use slopos_ostd::sync::spin::SpinLock;

use crate::{TestConfig, TestRunSummary, Verbosity};

static KERNEL_SUMMARY: SpinLock<TestRunSummary> = SpinLock::new(
    TestRunSummary {
        total: 0,
        passed: 0,
        failed: 0,
        skipped: 0,
        over_time: 0,
        panics: 0,
        elapsed_ms: 0,
    },
    slopos_ostd::lock_class!("KERNEL_SUMMARY", slopos_ostd::sync::LOCK_LEVEL_RESOURCE),
);

static KERNEL_RC: AtomicI32 = AtomicI32::new(0);

static KERNEL_CONFIG: SpinLock<TestConfig> = SpinLock::new(
    TestConfig {
        enabled: false,
        verbosity: Verbosity::Summary,
        warn_ms: 0,
        shutdown: false,
        stacktrace_demo: false,
        run_globs: KVec::new(),
        skip_globs: KVec::new(),
    },
    slopos_ostd::lock_class!("KERNEL_CONFIG", slopos_ostd::sync::LOCK_LEVEL_RESOURCE),
);

/// Whether `tests.shutdown=on` was set on the boot command line.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Whether `tests=on` was set on the boot command line.
static TESTS_ENABLED: AtomicBool = AtomicBool::new(false);

/// Called once from the boot init pipeline after `tests_run_all` returns;
/// later calls overwrite.
pub fn store_kernel_phase(summary: &TestRunSummary, rc: i32, cfg: &TestConfig) {
    *KERNEL_SUMMARY.lock() = *summary;
    *KERNEL_CONFIG.lock() = cfg.clone();
    KERNEL_RC.store(rc, Ordering::Release);
    TESTS_ENABLED.store(cfg.enabled, Ordering::Release);
    SHUTDOWN_REQUESTED.store(cfg.shutdown, Ordering::Release);
}

pub fn load_kernel_phase() -> (TestRunSummary, i32) {
    let summary = *KERNEL_SUMMARY.lock();
    let rc = KERNEL_RC.load(Ordering::Acquire);
    (summary, rc)
}

pub fn load_config() -> TestConfig {
    KERNEL_CONFIG.lock().clone()
}

pub fn tests_enabled() -> bool {
    TESTS_ENABLED.load(Ordering::Acquire)
}

pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Acquire)
}
