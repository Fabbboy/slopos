//! Userland-test runner.
//!
//! Lives in `slopos-core` for the spawn API, the per-task `TestReportRing` and
//! the wait helpers. The companion [`utest!`](crate::utest) macro lives here
//! too, so `slopos-testing` need not depend on `slopos-core` and cycle.

use slopos_abi::task::{
    INVALID_TASK_ID, TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE, TaskExitReason, TaskPriority,
};
use slopos_ostd::KVec;
use slopos_ostd::{catch_panic, klog_info};
use slopos_testing::{TestDesc, TestResult, ktap};

use crate::exec::{FdAction, spawn_program_with_attrs};
use slopos_sched::scheduler::{sleep_current_task_ms, task_wait_for};
use slopos_sched::task::{task_consume_zombie, task_find_by_id, task_peek_exit_info};
use slopos_sched::test_reports::TestReport;

/// Per-utest entry point installed in every `TestDesc::run`. Spawns the binary,
/// waits for it to terminate, drains its `SYSCALL_TEST_REPORT` ring into one
/// indented KTAP subtest line per report, then rolls up to a parent outcome.
/// `catch_panic!` turns a kernel-side panic along that path into a `Panic`
/// outcome for the parent utest rather than a dead harness.
pub fn run_thunk(desc: &'static TestDesc) -> TestResult {
    let bin = match desc.bin {
        Some(b) => b,
        None => {
            klog_info!("UTEST: '{}' missing bin path", desc.name);
            return TestResult::Fail;
        }
    };

    let argv_bytes: [&[u8]; 8] = [b"", b"", b"", b"", b"", b"", b"", b""];
    // argv is bounded to 8 to keep the frame under the stack gate; the macro
    // caps callers at the same number.
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
    // Init's pid/tid: the spawned utest resolves its stdio clone-actions
    // against init's console fds, and needs a real `parent_task_id` for
    // `notify_parent_of_child_exit` to deliver SIGCHLD.
    let (parent_table, parent_tid) = match slopos_sched::task_struct::Current::get() {
        Some(cur) => (
            cur.task()
                .process()
                .as_deref()
                .and_then(slopos_fs::fileio::FdTable::of),
            cur.id(),
        ),
        None => (None, INVALID_TASK_ID),
    };

    klog_info!("UTEST: starting '{}'", bin);

    // Inherit the harness's stdio so the test's KTAP output reaches serial.
    let stdio = [
        FdAction::Clone {
            src_fd: 0,
            target_fd: 0,
        },
        FdAction::Clone {
            src_fd: 1,
            target_fd: 1,
        },
        FdAction::Clone {
            src_fd: 2,
            target_fd: 2,
        },
    ];
    let pid = match spawn_program_with_attrs(
        bin.as_bytes(),
        argv,
        None,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE | TASK_FLAG_SYSTEM,
        &stdio,
        0,
        parent_table,
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

    // An owning handle held across the whole wait: the report ring lives on the
    // Task, and this is what stops the slot being recycled under the read.
    let Some(child) = task_find_by_id(pid) else {
        klog_info!("UTEST: '{}' pid={} vanished before the wait", bin, pid);
        return TestResult::Fail;
    };

    // TODO(tech-debt): `task_wait_for` can return before the target has run at
    // all, so the exit cell is polled as the ground truth — make the wait
    // edge-accurate and drop the poll.
    if !child.exit_info_is_set() {
        let _ = task_wait_for(pid);
    }
    let mut polled_ms: u32 = 0;
    const POLL_STEP_MS: u32 = 1;
    const POLL_LIMIT_MS: u32 = 5000;
    while !child.exit_info_is_set() {
        if polled_ms >= POLL_LIMIT_MS {
            klog_info!(
                "UTEST: '{}' pid={} exceeded {}ms poll cap with no exit value",
                bin,
                pid,
                POLL_LIMIT_MS
            );
            break;
        }
        sleep_current_task_ms(POLL_STEP_MS);
        polled_ms = polled_ms.saturating_add(POLL_STEP_MS);
    }

    // Snapshot exit info before draining reports: it transitions the slot from
    // Zombie to Terminated, which must happen before the slot can be reused.
    let exit_info = task_consume_zombie(pid).or_else(|| task_peek_exit_info(pid));

    let maybe_ring = child.take_test_reports();
    if maybe_ring.is_none() {
        klog_info!(
            "UTEST: '{}' pid={} produced no reports — binary crashed before reporting?",
            bin,
            pid
        );
        return TestResult::Fail;
    }

    // Move the entries out so the ring's KBox is released before the
    // per-subtest klog emissions.
    let report_vec: KVec<TestReport> = match maybe_ring {
        Some(mut ring) => ring.drain().unwrap_or_else(|_| KVec::new()),
        None => KVec::new(),
    };

    let mut sub_idx: u32 = 0;
    let mut sub_failed: u32 = 0;
    for r in report_vec.iter() {
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

    roll_up(bin, sub_failed, report_vec.len(), exit_info)
}

/// Decide the parent verdict: any failed subtest, or anything but a clean exit,
/// is a `Fail`. Userland is built `panic-strategy = abort`, so cases that
/// already reported must not vouch for ones that never ran; a signal death
/// arrives as `Normal` with the signal in the exit code, so the code — not the
/// reason — is what distinguishes it. Split out of `dispatch` to keep that
/// frame under the 2 KiB gate.
#[inline(never)]
fn roll_up(
    bin: &str,
    sub_failed: u32,
    report_count: usize,
    exit_info: Option<slopos_sched::exit_info::ExitInfo>,
) -> TestResult {
    if sub_failed > 0 {
        return TestResult::Fail;
    }
    let Some(info) = exit_info else {
        klog_info!("UTEST: '{}' exit info unavailable", bin);
        return TestResult::Fail;
    };
    if info.exit_reason != TaskExitReason::Normal || info.exit_code != 0 {
        klog_info!(
            "UTEST: '{}' did not exit cleanly: reason={} code={} after {} report(s)",
            bin,
            exit_reason_str(info.exit_reason),
            info.exit_code,
            report_count
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}
