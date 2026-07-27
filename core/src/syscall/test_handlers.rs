//! `SYSCALL_TEST_REPORT` and `SYSCALL_RUN_USERLAND_TESTS` handlers.

use slopos_abi::Errno;
use slopos_abi::syscall::{TEST_REPORT_MSG_MAX, TEST_REPORT_NAME_MAX};
use slopos_ostd::klog_info;
use slopos_testing::{
    TestRunSummary, kernel_phase_summary, tests_request_shutdown, tests_run_userland,
};

use slopos_sched::test_reports::{TestReport, alloc_ring, empty_report};

use crate::syscall::common::syscall_bounded_from_user;

define_syscall!(syscall_test_report
    (ctx, status_raw: u32, name_ptr: u64, name_requested: u64, msg_ptr: u64, msg_requested: u64)
    -> Result<(), Errno>
{
    if status_raw > 2 {
        return Err(Errno::EINVAL);
    }
    let status = status_raw as u8;

    if name_requested == 0 {
        return Err(Errno::EINVAL);
    }
    let mut name_buf = [0u8; TEST_REPORT_NAME_MAX];
    let name_len = syscall_bounded_from_user(
        &mut name_buf,
        name_ptr,
        name_requested,
        TEST_REPORT_NAME_MAX,
    )
    .map_err(|_| Errno::EFAULT)?;

    let mut msg_buf = [0u8; TEST_REPORT_MSG_MAX];
    let msg_len = if msg_requested == 0 {
        0usize
    } else {
        syscall_bounded_from_user(&mut msg_buf, msg_ptr, msg_requested, TEST_REPORT_MSG_MAX)
            .map_err(|_| Errno::EFAULT)?
    };

    let task = ctx.task();

    // Build the report before touching the lock: it is a stack value with no
    // allocation, and the critical section should cover nothing but the push.
    let mut report: TestReport = empty_report();
    report.status = status;
    report.name_len = name_len as u8;
    report.msg_len = msg_len as u8;
    report.name[..name_len].copy_from_slice(&name_buf[..name_len]);
    if msg_len > 0 {
        report.msg[..msg_len].copy_from_slice(&msg_buf[..msg_len]);
    }

    // Allocate outside the lock, then install under it. The drain side is a
    // foreign task (`task_take_test_reports` on a corpse), so the two need
    // mutual exclusion — but an allocator call inside the critical section is
    // the shape that deadlocks against the buddy's cross-CPU reuse path, so the
    // ring is built first and only the install is guarded. The re-check under
    // the guard makes a lost race drop the spare rather than the winner.
    if task.test_reports.lock().is_none() {
        let Ok(fresh) = alloc_ring() else {
            return Err(Errno::ENOMEM);
        };
        let mut slot = task.test_reports.lock();
        if slot.is_none() {
            *slot = Some(fresh);
        }
    }

    let mut slot = task.test_reports.lock();
    let Some(ring) = slot.as_mut() else {
        return Err(Errno::ENOMEM);
    };
    ring.push(report);

    Ok(())
});

define_syscall!(syscall_run_userland_tests (ctx) -> Result<(), Errno> {
    if !kernel_phase_summary::tests_enabled() {
        return Ok(());
    }

    klog_info!("TESTS: Running userland phase (init syscall)");

    let mut cfg = kernel_phase_summary::load_config();
    cfg.shutdown = false;

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

    let _ = utest_rc;
    let _ = kernel_rc;

    if kernel_phase_summary::shutdown_requested() {
        klog_info!("TESTS: Auto shutdown enabled after harness");
        tests_request_shutdown(total_failed as i32);
    }

    Ok(())
});

define_syscall!(syscall_test_panic (ctx) -> Result<(), Errno> {
    // Runtime-armed fault injection: without the `panic.recover_smoke`
    // boot flag this is indistinguishable from an unimplemented syscall,
    // so production images expose no user-reachable panic trigger.
    if !slopos_ostd::boot_flags::has_flag(slopos_ostd::boot_flags::BOOT_FLAG_PANIC_RECOVER_SMOKE) {
        return Err(Errno::ENOSYS);
    }
    let _ = ctx;
    panic!("test_panic: deliberate syscall-context panic (panic.recover_smoke)");
});
