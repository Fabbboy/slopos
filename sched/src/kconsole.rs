//! Diagnostic-console commands over the task table.
//!
//! These are the two the console exists for: what every task is doing, and —
//! for the ones that are not doing anything — where they stopped.

use slopos_ostd::kconsole::{KCMD_INFORMATIONAL, KConsole};
use slopos_ostd::string::bytes_as_str;
use slopos_ostd::{kline, ksymline};

use crate::task::{Task, TaskStatus, task_for_each_active, task_slot_census};

/// Frames walked per parked task.
///
/// Deep enough to name the blocking primitive and its caller, shallow enough
/// that a machine with many blocked tasks still fits the line budget.
const PARK_FRAMES: usize = 12;

slopos_ostd::kcommand! {
    name = tasks,
    key = b't',
    help = "task table: state, placement, block reason, park site",
    flags = KCMD_INFORMATIONAL,
    run = run_tasks,
}

slopos_ostd::kcommand! {
    name = blocked,
    key = b'w',
    help = "blocked tasks only, with symbolized stacks",
    flags = KCMD_INFORMATIONAL,
    run = run_blocked,
}

slopos_ostd::kcommand! {
    name = sched,
    key = b's',
    help = "scheduler counters, per-CPU and total",
    flags = KCMD_INFORMATIONAL,
    run = run_sched,
}

fn run_sched(kc: &mut KConsole<'_>) {
    let stats = crate::lifecycle::get_scheduler_stats();
    let tasks = crate::task::get_task_stats();
    kline!(
        kc,
        "sched: switches={} yields={} ready={} schedule_calls={}",
        stats.context_switches,
        stats.yields,
        stats.ready_tasks,
        stats.schedule_calls
    );
    kline!(
        kc,
        "tasks: created={} active={} switches={}",
        tasks.total_tasks,
        tasks.active_tasks,
        tasks.context_switches
    );
    // Bounded by CPU count, so this command cannot itself become the storm the
    // line budget exists to stop.
    for cpu in 0..slopos_ostd::cpu::x86_64::pcr::get_cpu_count() {
        let Some(per) = crate::lifecycle::get_percpu_scheduler_stats(cpu) else {
            continue;
        };
        kline!(
            kc,
            "  cpu {}: switches={} preemptions={} ready={}",
            cpu,
            per.switches,
            per.preemptions,
            per.ready_tasks
        );
    }
}

fn run_tasks(kc: &mut KConsole<'_>) {
    dump(kc, false);
}

fn run_blocked(kc: &mut KConsole<'_>) {
    dump(kc, true);
}

fn dump(kc: &mut KConsole<'_>, blocked_only: bool) {
    let (live, free, terminated, active) = task_slot_census();
    kline!(
        kc,
        "tasks: live={} active={} terminated={} free={}",
        live,
        active,
        terminated,
        free
    );
    task_for_each_active(|task| {
        if blocked_only && task.status() != TaskStatus::Blocked {
            return;
        }
        // The walk is unbounded in the number of tasks, so it checks the
        // budget rather than trusting `line` to absorb the overflow: stopping
        // between records beats stopping mid-record.
        if kc.budget_left() == 0 {
            return;
        }
        dump_one(kc, task);
    });
}

fn dump_one(kc: &mut KConsole<'_>, t: &Task) {
    kline!(
        kc,
        "  task {:>3} '{}' status={:?} reason={:?} placement={:?} on_cpu={} pid={} pgid={} sid={} last_run={}",
        t.task_id,
        bytes_as_str(&t.name),
        t.status(),
        t.load_block_reason(),
        t.sched_placement(),
        t.on_cpu(),
        t.process_id,
        t.pgid(),
        t.sid(),
        t.last_run_timestamp(),
    );

    if t.status() != TaskStatus::Blocked {
        return;
    }

    let (rip, rsp) = t.switch_ctx_rip_rsp();
    let rbp = t.switch_ctx_rbp();
    ksymline!(kc, rip, "    parked rsp=0x{:x} rbp=0x{:x} at ", rsp, rbp);
    if rbp == 0 {
        return;
    }

    // The task is parked, so its saved frame pointer is stable for the walk.
    // Each read goes through the fault-recoverable probe, because a canonical
    // kernel address is not proof of a mapped one.
    let mut entries = [slopos_ostd::stacktrace::StacktraceEntry {
        frame_pointer: 0,
        return_address: 0,
    }; PARK_FRAMES];
    let captured = slopos_ostd::stacktrace::stacktrace_capture_from(
        rbp,
        entries.as_mut_ptr(),
        entries.len() as core::ffi::c_int,
    );
    for entry in entries.iter().take(captured.max(0) as usize) {
        ksymline!(kc, entry.return_address, "      ");
    }
}
