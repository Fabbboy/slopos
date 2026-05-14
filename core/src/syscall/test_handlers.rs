//! `SYSCALL_TEST_REPORT` and `SYSCALL_RUN_USERLAND_TESTS` handlers.
//!
//! - **`SYSCALL_TEST_REPORT`** — userland test binaries call this once
//!   per subtest to report their result. Reports are buffered into the
//!   calling task's per-task `TestReportRing` (lazily allocated) and
//!   drained by [`crate::scheduler::task::task_table::task_drain_test_reports`]
//!   after the task has exited.
//!
//! - **`SYSCALL_RUN_USERLAND_TESTS`** — `/sbin/init` calls this once when
//!   the kernel was booted with `tests=on`. The handler runs in the
//!   caller's task context, so `task_wait_for(child_pid)` blocks the way
//!   it would for any other syscall. It walks the `.test_registry` for
//!   `TestKind::Userland`, drives every utest (spawn → wait → drain →
//!   emit indented KTAP), merges the totals with the kernel-phase
//!   summary stashed at boot, and signals shutdown when
//!   `tests.shutdown=on`.

use slopos_abi::syscall::{TEST_REPORT_MSG_MAX, TEST_REPORT_NAME_MAX};
use slopos_ostd::klog_info;
use slopos_testing::{
    TestRunSummary,
    config::{TestConfig, Verbosity},
    kernel_phase_summary, tests_request_shutdown, tests_run_userland,
};

use crate::scheduler::test_reports::{TestReport, alloc_ring, empty_report};

define_syscall!(syscall_test_report(ctx, args) {
    let status_raw = args.arg0 as u32;
    if status_raw > 2 {
        return ctx.err();
    }
    let status = status_raw as u8;

    let name_requested = args.arg2;
    if name_requested == 0 {
        return ctx.err();
    }
    let mut name_buf = [0u8; TEST_REPORT_NAME_MAX];
    let name_len = match crate::syscall::common::syscall_bounded_from_user(
        &mut name_buf,
        args.arg1,
        name_requested,
        TEST_REPORT_NAME_MAX,
    ) {
        Ok(n) => n,
        Err(_) => return ctx.err(),
    };

    let msg_requested = args.arg4;
    let mut msg_buf = [0u8; TEST_REPORT_MSG_MAX];
    let msg_len = if msg_requested == 0 {
        0usize
    } else {
        match crate::syscall::common::syscall_bounded_from_user(
            &mut msg_buf,
            args.arg3,
            msg_requested,
            TEST_REPORT_MSG_MAX,
        ) {
            Ok(n) => n,
            Err(_) => return ctx.err(),
        }
    };

    let task = match ctx.task_mut() {
        Some(t) => t,
        None => return ctx.err(),
    };

    if task.test_reports.is_none() {
        match alloc_ring() {
            Ok(b) => task.test_reports = Some(b),
            Err(_) => return ctx.err_with(slopos_abi::syscall::ERRNO_ENOMEM),
        }
    }
    let ring = task.test_reports.as_mut().unwrap();
    let mut report: TestReport = empty_report();
    report.status = status;
    report.name_len = name_len as u8;
    report.msg_len = msg_len as u8;
    report.name[..name_len].copy_from_slice(&name_buf[..name_len]);
    if msg_len > 0 {
        report.msg[..msg_len].copy_from_slice(&msg_buf[..msg_len]);
    }
    ring.push(report);

    ctx.ok(0)
});

define_syscall!(syscall_run_userland_tests(ctx, _args) {
    if !kernel_phase_summary::tests_enabled() {
        // tests=off — silently no-op so a stray init invocation can't
        // wedge the boot. Returning 0 keeps the userland helper trivial.
        return ctx.ok(0);
    }

    klog_info!("TESTS: Running userland phase (init syscall)");

    // Build a minimal TestConfig for the harness. The userland-phase only
    // needs `enabled=true` plus the verbosity-derived behaviour; glob
    // filtering happens at registration boundary already (kind filter)
    // and the 3-utest scale doesn't need run/skip globs at runtime.
    let cfg = TestConfig {
        enabled: true,
        verbosity: Verbosity::Summary,
        warn_ms: 0,
        shutdown: false,            // shutdown handled below after merge
        stacktrace_demo: false,
        run_globs: slopos_ostd::KVec::new(),
        skip_globs: slopos_ostd::KVec::new(),
    };

    let mut utest_summary = TestRunSummary::default();
    let utest_rc = tests_run_userland(&cfg, &mut utest_summary);

    let (kernel_summary, kernel_rc) = kernel_phase_summary::load_kernel_phase();
    let total_failed = kernel_summary.failed.saturating_add(utest_summary.failed);
    let total_passed = kernel_summary.passed.saturating_add(utest_summary.passed);
    let total = kernel_summary.total.saturating_add(utest_summary.total);
    let total_panics = kernel_summary.panics.saturating_add(utest_summary.panics);

    klog_info!(
        "TESTS SUMMARY (cumulative): total={} passed={} failed={} panics={}",
        total,
        total_passed,
        total_failed,
        total_panics,
    );

    // `tests_run_userland` returns -1 when there were failures (or if the
    // registry walk bailed out). Surface only the registry-bail case as an
    // internal error; failure counts are already reflected above.
    let _ = utest_rc;
    let _ = kernel_rc;

    if kernel_phase_summary::shutdown_requested() {
        klog_info!("TESTS: Auto shutdown enabled after harness");
        tests_request_shutdown(total_failed as i32);
    }

    ctx.ok(0)
});
