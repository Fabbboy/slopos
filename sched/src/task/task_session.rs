use slopos_abi::signal::SIGCHLD;
use slopos_abi::syscall::TtyIndex;
use slopos_ostd::task::{ProcessGroup, Session};
use slopos_ostd::{KArc, KWeak};

use super::task_ops::{task_signal_post, task_wake_all_waiters};
use super::task_table::{task_find_by_id, task_for_each_active, with_task_manager};
use super::{INVALID_TASK_ID, Task};

/// Resolve a live weak handle to the process group `pgid` names, by
/// downgrading a current member's strong handle. `None` when no live member
/// carries a group object for `pgid`. Runs under the task-manager lock so it
/// never races slot recycle (which drops the member handle under the same lock).
pub fn pgrp_handle_for_pgid(pgid: u32) -> Option<KWeak<ProcessGroup>> {
    if pgid == 0 {
        return None;
    }
    with_task_manager(|mgr| {
        for task in mgr.iter_tasks() {
            if task.pgid() == pgid {
                if let Some(pg) = task.process_group.load() {
                    return Some(KArc::downgrade(&pg));
                }
            }
        }
        None
    })
}

/// Resolve a live weak handle to the session `sid` names, by downgrading a
/// current member's session handle. `None` when no live member carries a group
/// object for `sid`.
pub fn session_handle_for_sid(sid: u32) -> Option<KWeak<Session>> {
    if sid == 0 {
        return None;
    }
    with_task_manager(|mgr| {
        for task in mgr.iter_tasks() {
            if task.sid() == sid {
                if let Some(pg) = task.process_group.load() {
                    return Some(KArc::downgrade(pg.session()));
                }
            }
        }
        None
    })
}

pub fn task_clear_controlling_tty_for_session(session_id: u32, tty: TtyIndex) -> usize {
    if session_id == 0 {
        return 0;
    }

    let mut cleared = 0usize;
    task_for_each_active(|task| {
        // The walk hands us a live registry entry as `&Task`; both halves are
        // `&self` operations, so the accessor was a null check around nothing.
        if task.sid() == session_id && task.clear_controlling_tty_if(tty) {
            cleared = cleared.saturating_add(1);
        }
    });
    cleared
}

pub(super) fn release_task_dependents(completed_task_id: u32) {
    let Some(task) = task_find_by_id(completed_task_id) else {
        return;
    };
    // Caller (mark_task_terminated) holds the task pointer stable via
    // the task-manager lock window in which it runs; additionally,
    // the per-task `waiters` queue is a `WaitQueue` whose internal
    // SpinLock makes wake_all interrupt-safe and serialises against
    // any concurrent waiter registration. The SpinLock pair (this
    // side + the waiter's `is_set` check inside `wait_event`) is the
    // bidirectional full barrier that pairs with the Release
    // `try_set` published just before this call.
    task_wake_all_waiters(&task);
}

pub(super) fn notify_parent_of_child_exit(task: &Task) {
    let task_id = task.task_id;
    let tgid = task.tgid;
    let parent_task_id = task.parent_task_id();

    if parent_task_id == INVALID_TASK_ID || parent_task_id == task_id {
        return;
    }

    if tgid != task_id {
        return;
    }

    let Some(parent) = task_find_by_id(parent_task_id) else {
        return;
    };

    let _ = task_signal_post(&parent, SIGCHLD);
    // Wakes `waitpid(-1)`. Published unconditionally, not only when a waiter
    // exists: `wait_event` re-checks its predicate on every wake, and the
    // waiter registers before it scans, so a publish that races the
    // registration costs a re-scan rather than a lost wakeup.
    slopos_ostd::sync::BUS.publish(slopos_ostd::task::ops::any_child_exit_event(parent_task_id));
}
