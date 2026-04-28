//! Userland-test runner.
//!
//! Lives in `slopos-core` because it needs the spawn API, the per-task
//! `TestReportRing`, and the `task_wait_for`/`task_get_exit_record`
//! helpers — all of which are core-internal. The companion
//! [`utest!`](crate::utest) macro emits a `TestDesc` whose `run` thunk
//! dispatches into [`run_thunk`]. The macro lives in this crate too so
//! `slopos-testing` does not need to depend on `slopos-core` (which would
//! cycle: core already deps testing for the `stest!` macros).

use slopos_abi::task::{
    INVALID_PROCESS_ID, INVALID_TASK_ID, TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE, TaskExitReason,
    TaskExitRecord, TaskFaultReason, TaskPriority,
};
use slopos_testing::{TestDesc, TestResult, ktap};
use slopos_utils::{catch_panic, klog_info};

use crate::exec::spawn_program_with_attrs;
use crate::sched::scheduler_get_current_task;
use crate::scheduler::scheduler::task_wait_for;
use crate::task::{task_drain_test_reports, task_find_by_id, task_get_exit_record};

/// Per-utest entry point installed in every `TestDesc::run` produced by the
/// [`utest!`](crate::utest) macro. Spawns the binary, waits for it to
/// terminate, drains its `SYSCALL_TEST_REPORT` ring, emits one indented
/// KTAP subtest line per report, then rolls up to a parent outcome.
///
/// Wrapped in `catch_panic!` so a kernel-side panic inside spawn / wait /
/// drain does not crash the harness — it is reported as a `Panic` outcome
/// for the parent utest, the next test still runs.
pub fn run_thunk(desc: &'static TestDesc) -> TestResult {
    let bin = match desc.bin {
        Some(b) => b,
        None => {
            klog_info!("UTEST: '{}' missing bin path", desc.name);
            return TestResult::Fail;
        }
    };

    // Build argv as &[&[u8]] from the static &[&'static str].
    let argv_bytes: [&[u8]; 8] = [b"", b"", b"", b"", b"", b"", b"", b""];
    // Bound argv to 8 to keep the stack frame bounded; the macro's literal
    // form caps callers at the same number through compile-time matching.
    let mut argv_storage = argv_bytes;
    let argv_len = desc.argv.len().min(argv_storage.len());
    for i in 0..argv_len {
        argv_storage[i] = desc.argv[i].as_bytes();
    }
    let argv_slice: &[&[u8]] = &argv_storage[..argv_len];
    let argv_opt = if argv_slice.is_empty() {
        None
    } else {
        Some(argv_slice)
    };

    let rc: i32 = catch_panic!({
        match dispatch(bin, argv_opt) {
            TestResult::Pass => 0,
            TestResult::Fail => 1,
            _ => 2,
        }
    });

    match rc {
        0 => TestResult::Pass,
        1 => TestResult::Fail,
        _ => TestResult::Panic,
    }
}

fn exit_reason_str(reason: TaskExitReason) -> &'static str {
    match reason {
        TaskExitReason::None => "None",
        TaskExitReason::Normal => "Normal",
        TaskExitReason::UserFault => "UserFault",
        TaskExitReason::Kernel => "Kernel",
    }
}

fn dispatch(bin: &str, argv: Option<&[&[u8]]>) -> TestResult {
    // Pull init's pid/tid out of the current task so spawned utests inherit
    // the fd table and have a real `parent_task_id` set. Without this, the
    // spawned task has `parent_task_id = INVALID_TASK_ID` and
    // `inherit_fds_from = INVALID_PROCESS_ID` — which leaves it with no
    // stdin/stdout/stderr and matches the `notify_parent_of_child_exit`
    // early-return path. Wakeup still flows through `release_task_dependents`
    // (which scans for `waiting_on==completed_id`), but a child with no
    // fd table can fault before reporting any subtest results.
    let (parent_pid, parent_tid) = unsafe {
        let cur = scheduler_get_current_task();
        if cur.is_null() {
            (INVALID_PROCESS_ID, INVALID_TASK_ID)
        } else {
            ((*cur).process_id, (*cur).task_id)
        }
    };

    let pid = match spawn_program_with_attrs(
        bin.as_bytes(),
        argv,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE | TASK_FLAG_SYSTEM,
        parent_pid,
        parent_tid,
    ) {
        Ok(pid) => pid,
        Err(e) => {
            klog_info!(
                "UTEST: spawn '{}' failed: {:?} (binary missing from FS image?)",
                bin,
                e
            );
            return TestResult::Fail;
        }
    };

    // Hold a refcount on the spawned task across the wait so that
    // `reap_zombies` (which fires on every CPU's idle iteration) cannot
    // recycle the slot — and zero out `test_reports` via
    // `Task::reset_in_place` — between the child exiting and us draining
    // the per-task report ring. Without this hold, the drained ring is
    // empty by the time we look.
    let task_ptr = task_find_by_id(pid);
    if !task_ptr.is_null() {
        unsafe {
            (*task_ptr).inc_ref();
        }
    }

    // Mirror `syscall_waitpid`: try the exit-record cache first, and only
    // park on `task_wait_for` if the child is still live. Two reasons:
    // (1) it sidesteps the case where the child terminated before we
    //     reach `task_wait_for` and the wake fired before our
    //     `prepare_to_wait`, which would leave us blocked forever; and
    // (2) it's the same shape as the production waitpid path, so future
    //     scheduler invariants stay in lockstep.
    let mut record = TaskExitRecord {
        task_id: 0,
        exit_reason: TaskExitReason::None,
        fault_reason: TaskFaultReason::None,
        exit_code: 0,
    };
    let mut exit_rc = task_get_exit_record(pid, &mut record as *mut _);
    if exit_rc != 0 {
        let _ = task_wait_for(pid);
        exit_rc = task_get_exit_record(pid, &mut record as *mut _);
    }
    let reports = task_drain_test_reports(pid);

    // Drop the post-wait refcount. The slot is now reusable; the next
    // `reap_zombies` pass will recycle it.
    if !task_ptr.is_null() {
        unsafe {
            (*task_ptr).dec_ref();
        }
    }

    let mut sub_idx: u32 = 0;
    let mut sub_failed: u32 = 0;
    for r in reports.iter() {
        sub_idx += 1;
        let name_slice = &r.name[..r.name_len as usize];
        let msg_slice = &r.msg[..r.msg_len as usize];
        let name = core::str::from_utf8(name_slice).unwrap_or("<non-utf8>");
        let msg = core::str::from_utf8(msg_slice).unwrap_or("");
        match r.status {
            0 => ktap::emit_subtest_ok(sub_idx, name),
            1 => {
                sub_failed += 1;
                ktap::emit_subtest_not_ok(sub_idx, name, msg);
            }
            2 => ktap::emit_subtest_skip(sub_idx, name),
            other => {
                sub_failed += 1;
                klog_info!("UTEST: '{}' subtest {} bad status {}", bin, name, other);
                ktap::emit_subtest_not_ok(sub_idx, name, "invalid status");
            }
        }
    }

    // Roll-up:
    //   any Fail subtest reported  → parent Fail
    //   no reports + non-zero exit → parent Fail (binary crashed before
    //                                reporting)
    //   else                       → parent Pass (exit code may still be
    //                                non-zero from `slibc::test_harness::run`'s
    //                                failure-count semantics; trust the
    //                                drained subtest verdicts)
    if sub_failed > 0 {
        return TestResult::Fail;
    }
    let exited_normally =
        exit_rc == 0 && record.exit_reason == TaskExitReason::Normal && record.exit_code == 0;
    if !exited_normally && reports.is_empty() {
        klog_info!(
            "UTEST: '{}' exited rc={} reason={} code={} with no reports",
            bin,
            exit_rc,
            exit_reason_str(record.exit_reason),
            record.exit_code
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}
