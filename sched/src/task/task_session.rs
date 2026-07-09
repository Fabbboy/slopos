use core::ffi::c_void;

use slopos_abi::signal::SIGCHLD;
use slopos_abi::syscall::TtyIndex;
use slopos_ostd::task::{ProcessGroup, Session};
use slopos_ostd::{KArc, KWeak};

use super::task_accessors::{
    task_clear_controlling_tty_for, task_id_of, task_parent_task_id, task_signal_post, task_tgid,
    task_wake_all_waiters,
};
use super::task_table::{task_find_by_id, task_iterate_active, with_task_manager};
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
            if task.pgid == pgid {
                if let Some(pg) = task.process_group.as_ref() {
                    return Some(KArc::downgrade(pg));
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
            if task.sid == sid {
                if let Some(pg) = task.process_group.as_ref() {
                    return Some(KArc::downgrade(pg.session()));
                }
            }
        }
        None
    })
}

struct ClearControllingTtyContext {
    session_id: u32,
    tty: TtyIndex,
    cleared: usize,
}

fn clear_controlling_tty_for_session_task(task: *mut Task, context: *mut c_void) {
    if task.is_null() || context.is_null() {
        return;
    }

    let Some(ctx) =
        slopos_ostd::util::ptr_buf::try_void_ctx_mut::<ClearControllingTtyContext>(context)
    else {
        return;
    };
    if task_clear_controlling_tty_for(task, ctx.session_id, ctx.tty) {
        ctx.cleared = ctx.cleared.saturating_add(1);
    }
}

pub fn task_clear_controlling_tty_for_session(session_id: u32, tty: TtyIndex) -> usize {
    if session_id == 0 {
        return 0;
    }

    let mut ctx = ClearControllingTtyContext {
        session_id,
        tty,
        cleared: 0,
    };
    task_iterate_active(
        Some(clear_controlling_tty_for_session_task),
        (&mut ctx as *mut ClearControllingTtyContext).cast(),
    );
    ctx.cleared
}

pub(super) fn release_task_dependents(completed_task_id: u32) {
    let task = task_find_by_id(completed_task_id);
    if task.is_null() {
        return;
    }
    // Caller (mark_task_terminated) holds the task pointer stable via
    // the task-manager lock window in which it runs; additionally,
    // the per-task `waiters` queue is a `WaitQueue` whose internal
    // SpinLock makes wake_all interrupt-safe and serialises against
    // any concurrent waiter registration. The SpinLock pair (this
    // side + the waiter's `is_set` check inside `wait_event`) is the
    // bidirectional full barrier that pairs with the Release
    // `try_set` published just before this call.
    task_wake_all_waiters(task);
}

pub(super) fn notify_parent_of_child_exit(task_ptr: *mut Task) {
    if task_ptr.is_null() {
        return;
    }

    let task_id = task_id_of(task_ptr).unwrap_or(INVALID_TASK_ID);
    let tgid = task_tgid(task_ptr).unwrap_or(INVALID_TASK_ID);
    let parent_task_id = task_parent_task_id(task_ptr).unwrap_or(INVALID_TASK_ID);

    if parent_task_id == INVALID_TASK_ID || parent_task_id == task_id {
        return;
    }

    if tgid != task_id {
        return;
    }

    let parent = task_find_by_id(parent_task_id);
    if parent.is_null() {
        return;
    }

    let _ = task_signal_post(parent, SIGCHLD);
}
