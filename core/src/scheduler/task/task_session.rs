use core::ffi::c_void;
use core::sync::atomic::Ordering;

use slopos_abi::signal::{SIGCHLD, sig_bit};
use slopos_abi::syscall::TtyIndex;
use slopos_utils::klog_info;

use super::super::scheduler;
use super::task_state::{task_is_blocked, task_is_will_block};
use super::task_table::{pool_high_water, task_find_by_id, task_iterate_active, with_task_manager};
use super::{INVALID_TASK_ID, Task};

struct ClearControllingTtyContext {
    session_id: u32,
    tty: TtyIndex,
    cleared: usize,
}

fn clear_controlling_tty_for_session_task(task: *mut Task, context: *mut c_void) {
    if task.is_null() || context.is_null() {
        return;
    }

    let ctx = unsafe { &mut *context.cast::<ClearControllingTtyContext>() };
    unsafe {
        if (*task).sid == ctx.session_id && (*task).controlling_tty == Some(ctx.tty) {
            (*task).controlling_tty = None;
            ctx.cleared = ctx.cleared.saturating_add(1);
        }
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
    // Collect blocked-on-`completed_task_id` task pointers into a
    // heap-backed KVec. A stack-resident array sized for the whole
    // pool would cost tens of KiB per call — well over the 2 KiB
    // frame gate. Scratch capacity is sized to the pool high-water
    // mark so a lightly-loaded system pays for a small allocation.
    let capacity = pool_high_water().max(1);
    let mut candidates: slopos_ostd::KVec<usize> = match slopos_ostd::KVec::with_capacity(capacity)
    {
        Ok(v) => v,
        Err(_) => return,
    };
    with_task_manager(|mgr| {
        for dependent in mgr.iter_tasks_mut() {
            let dep_ptr = dependent as *mut Task;
            // Include both Blocked and WillBlock — the latter covers the
            // tiny race window between `task_set_waiting_on()` and the
            // `WillBlock→Blocked` CAS in `block_current_task`. Without
            // this, a wake fired in that window finds the waiter "not
            // blocked yet" and skips it; the waiter then completes its
            // CAS to Blocked and sleeps forever.
            if !task_is_blocked(dep_ptr) && !task_is_will_block(dep_ptr) {
                continue;
            }
            if dependent.waiting_on.load(Ordering::Acquire) != completed_task_id {
                continue;
            }
            let _ = candidates.push(dep_ptr as usize);
        }
    });

    for addr in candidates.iter() {
        let task = *addr as *mut Task;
        let task_id = unsafe { (*task).task_id };

        if scheduler::try_wake_from_task_wait(task, completed_task_id) {
            klog_info!(
                "release_task_dependents: Woke task {} (was waiting on {})",
                task_id,
                completed_task_id
            );
        }
    }
}

pub(super) fn notify_parent_of_child_exit(task_ptr: *mut Task) {
    if task_ptr.is_null() {
        return;
    }

    let (task_id, tgid, parent_task_id) = unsafe {
        (
            (*task_ptr).task_id,
            (*task_ptr).tgid,
            (*task_ptr).parent_task_id,
        )
    };

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

    unsafe {
        (*parent)
            .signal_pending
            .fetch_or(sig_bit(SIGCHLD), Ordering::AcqRel);
    }
}
