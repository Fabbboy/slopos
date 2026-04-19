#![no_std]

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_drivers::interrupt_test::interrupt_test_request_shutdown;
use slopos_sync::StateFlag;
pub use slopos_testing::{
    HARNESS_MAX_SUITES, TestConfig, TestRunSummary, TestSuiteDesc, TestSuiteResult, Verbosity,
    measure_elapsed_ms,
};
use slopos_utils::klog_info;

// The per-subsystem test suites carry multi-KiB stack frames (XSAVE
// regions, pattern buffers, etc.) that would trip the production
// stack-size gate. Gate the modules themselves so they are only
// compiled when the kernel is built with `kernel/builtin-tests`.
#[cfg(feature = "builtin-tests")]
pub mod exception_tests;
#[cfg(feature = "builtin-tests")]
pub mod fpu_tests;
#[cfg(feature = "builtin-tests")]
pub mod xsave_tests;

pub const TESTS_MAX_SUITES: usize = HARNESS_MAX_SUITES;

static PANIC_SEEN: StateFlag = StateFlag::new();
static PANIC_REPORTED: AtomicBool = AtomicBool::new(false);

pub fn tests_reset_panic_state() {
    PANIC_SEEN.set_inactive();
    PANIC_REPORTED.store(false, Ordering::Relaxed);
}

#[cfg(not(feature = "builtin-tests"))]
pub fn tests_run_all(
    _config: *const TestConfig,
    _summary: *mut TestRunSummary,
    _registry_start: *const TestSuiteDesc,
    _registry_end: *const TestSuiteDesc,
) -> i32 {
    // No-op in production: `TestRunSummary` is 6 KiB on the kernel
    // stack, tripping the stack-size gate. The real body compiles
    // under the `builtin-tests` feature (which lifts the gate via
    // scripts/build_kernel.sh).
    0
}

#[cfg(feature = "builtin-tests")]
pub fn tests_run_all(
    config: *const TestConfig,
    summary: *mut TestRunSummary,
    registry_start: *const TestSuiteDesc,
    registry_end: *const TestSuiteDesc,
) -> i32 {
    if config.is_null() {
        return -1;
    }

    let mut local_summary = TestRunSummary::default();
    let summary = if summary.is_null() {
        &mut local_summary
    } else {
        unsafe {
            *summary = TestRunSummary::default();
            &mut *summary
        }
    };

    let cfg = unsafe { &*config };
    if !cfg.enabled {
        klog_info!("TESTS: Harness disabled");
        return 0;
    }

    klog_info!("TESTS: Starting test suites");

    let start_cycles = slopos_arch::tsc::rdtsc();
    let mut idx = 0usize;
    let mut cursor = registry_start;
    while cursor < registry_end {
        if PANIC_SEEN.is_active() {
            summary.unexpected_exceptions = summary.unexpected_exceptions.saturating_add(1);
            summary.failed = summary.failed.saturating_add(1);
            if !PANIC_REPORTED.swap(true, Ordering::Relaxed) {
                klog_info!("TESTS: panic flagged, stopping suite execution");
            }
            break;
        }

        let desc = unsafe { &*cursor };

        let suite_start = slopos_arch::tsc::rdtsc();
        let mut res = TestSuiteResult::default();
        res.name = desc.name;

        if let Some(run) = desc.run {
            let config_ptr = config as *const ();
            // Call the suite runner directly — no outer catch_panic!.
            // Individual tests use catch_panic! internally (run_single_test)
            // to catch intentional panics. Unexpected suite-level panics
            // propagate to the default panic handler which exits QEMU with
            // a failure code in test mode, avoiding deadlocks from longjmp
            // through held locks or corrupted state.
            run(config_ptr, &mut res);
        }

        if PANIC_SEEN.is_active() {
            res.unexpected_exceptions = res.unexpected_exceptions.saturating_add(1);
            res.failed = res.failed.saturating_add(1);
        }

        if cfg.timeout_ms != 0 {
            let elapsed = measure_elapsed_ms(suite_start, slopos_arch::tsc::rdtsc());
            if elapsed > cfg.timeout_ms {
                res.timed_out = 1;
                res.failed = res.failed.saturating_add(1);
                if !PANIC_REPORTED.swap(true, Ordering::Relaxed) {
                    klog_info!("TESTS: suite timeout exceeded");
                }
            }
        }

        if summary.suite_count < TESTS_MAX_SUITES {
            summary.suites[summary.suite_count] = res;
            summary.suite_count += 1;
        }

        klog_info!(
            "SUITE{} total={} pass={} fail={} elapsed={}ms",
            idx as u32,
            res.total,
            res.passed,
            res.failed,
            res.elapsed_ms,
        );
        summary.add_suite_result(&res);

        idx += 1;
        cursor = unsafe { cursor.add(1) };
    }
    let end_cycles = slopos_arch::tsc::rdtsc();
    let overall_ms = measure_elapsed_ms(start_cycles, end_cycles);
    if overall_ms > summary.elapsed_ms {
        summary.elapsed_ms = overall_ms;
    }

    klog_info!(
        "TESTS SUMMARY: total={} passed={} failed={} elapsed_ms={}",
        summary.total_tests,
        summary.passed,
        summary.failed,
        summary.elapsed_ms,
    );

    // LUF (Lazy Unmap Flush) counters aggregated over all CPUs — proves
    // that cross-CPU coherence is actually flowing through the ring
    // and the drain IPI, not silently short-circuiting. Non-zero
    // `queued` on a fork-heavy or munmap-heavy run means the migration
    // is live; `reuse_drains` reflects how many times a frame was
    // reclaimed while still carrying a deferred entry.
    {
        let mut q = 0u64;
        let mut d = 0u64;
        let mut r = 0u64;
        let mut o = 0u64;
        for cpu in 0..slopos_arch::pcr::MAX_CPUS {
            q = q.saturating_add(slopos_mm::mmu::luf::queued_count(cpu));
            d = d.saturating_add(slopos_mm::mmu::luf::deferred_saves_count(cpu));
            r = r.saturating_add(slopos_mm::mmu::luf::reuse_drains_count(cpu));
            o = o.saturating_add(slopos_mm::mmu::luf::overflow_drains_count(cpu));
        }
        klog_info!(
            "LUF SUMMARY: queued={} reuse_drains={} overflow_drains={} deferred_saves={}",
            q,
            r,
            o,
            d,
        );
    }

    if summary.failed == 0 { 0 } else { -1 }
}

pub fn tests_request_shutdown(failed: i32) {
    interrupt_test_request_shutdown(failed);
}

pub fn tests_mark_panic() {
    PANIC_SEEN.set_active();
    if !PANIC_REPORTED.swap(true, Ordering::Relaxed) {
        klog_info!("TESTS: panic observed");
    }
}
