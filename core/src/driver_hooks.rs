use core::ops::ControlFlow;

use slopos_abi::signal::{SIG_IGN, sig_bit};
use slopos_kernel_services::driver_runtime::{
    DriverRuntimeServices, register_driver_runtime_services,
};

use crate::irq;
use slopos_ostd::KArc;
use slopos_ostd::task::ProcessGroup;
use slopos_sched::scheduler;
use slopos_sched::task::{
    self, task_has_deliverable_signal, task_signal_blocked, task_signal_handler, task_signal_post,
};
use slopos_sched::task_struct::Current;

fn runtime_current_task_pgrp_handle() -> Option<slopos_ostd::KWeak<ProcessGroup>> {
    let current = Current::get()?;
    current.task().process_group.as_ref().map(KArc::downgrade)
}

// ---------------------------------------------------------------------------
// Adapter functions — only for service methods that need type conversion or
// non-trivial logic.  Pure 1:1 forwards are assigned directly in the static
// service table below.
// ---------------------------------------------------------------------------

/// Wake the task named by `task_id`.
///
/// The id is resolved through the registry rather than dereferenced: the wait
/// queue hands this back on an arbitrary CPU at an arbitrary later time, and a
/// waiter killed while parked never unwinds its own stack, so its node can
/// outlive it. A weak upgrade answers "already gone" where a pointer would have
/// named freed memory. [`scheduler::unblock_task_id`] is that lookup — this hook
/// is now only the service-table shape around it.
fn runtime_unblock_task(task_id: u32) -> i32 {
    scheduler::unblock_task_id(task_id)
}

/// Post `signum` to every member of `pgid`, waking blocked members. True if at
/// least one member matched.
fn runtime_signal_process_group(pgid: u32, signum: u8) -> bool {
    if pgid == 0 {
        return false;
    }

    let mut matched = false;
    task::task_for_each_active(|task| {
        if task.pgid != pgid {
            return;
        }
        if task_signal_post(task.as_ptr(), signum) {
            let _ = scheduler::unblock_task(task.arc());
        }
        matched = true;
    });
    matched
}

/// Post `signum` to every member of session `sid`, waking blocked members. True
/// if at least one member matched.
fn runtime_signal_session(sid: u32, signum: u8) -> bool {
    if sid == 0 {
        return false;
    }

    let mut matched = false;
    task::task_for_each_active(|task| {
        if task.sid != sid {
            return;
        }
        if task_signal_post(task.as_ptr(), signum) {
            let _ = scheduler::unblock_task(task.arc());
        }
        matched = true;
    });
    matched
}

// ---------------------------------------------------------------------------
// Check if a process group exists within a given session.
// ---------------------------------------------------------------------------

fn runtime_pgrp_exists_in_session(pgid: u32, sid: u32) -> bool {
    if pgid == 0 || sid == 0 {
        return false;
    }

    let mut found = false;
    task::task_try_for_each_active(|task| {
        if task.pgid == pgid && task.sid == sid {
            found = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    found
}

// ---------------------------------------------------------------------------
// Check if the current task has a signal blocked or set to SIG_IGN.
// ---------------------------------------------------------------------------

fn runtime_is_current_signal_blocked_or_ignored(signum: u8) -> bool {
    let Some(current) = Current::get() else {
        return false;
    };
    let bit = sig_bit(signum);
    if bit == 0 {
        return false; // invalid signal number
    }
    let task = current.as_ptr();
    if let Some(blocked) = task_signal_blocked(task) {
        if (blocked & bit) != 0 {
            return true;
        }
    }
    let idx = (signum as usize).wrapping_sub(1);
    if task_signal_handler(task, idx) == Some(SIG_IGN) {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Check if the current task has any deliverable (pending
// and not blocked) signals.  Used by the TTY read path to detect signal
// interruption and return ERESTARTSYS.
// ---------------------------------------------------------------------------

fn runtime_has_pending_signal() -> bool {
    Current::get().is_some_and(|current| task_has_deliverable_signal(current.as_ptr()))
}

// ---------------------------------------------------------------------------
// Check if a process group is orphaned within a session.
//
// A process group is orphaned if no member of the group has a parent that is
// in a *different* process group within the *same* session.  When an orphaned
// background pgrp tries to perform a terminal operation that would generate
// SIGTTOU, POSIX requires returning EIO instead (since there is no parent to
// continue the stopped group).
// ---------------------------------------------------------------------------

fn runtime_is_pgrp_orphaned(pgid: u32, sid: u32) -> bool {
    if pgid == 0 || sid == 0 {
        return false; // no valid group/session — not orphaned by definition
    }

    // First check that the pgrp even has members in the session.
    if !runtime_pgrp_exists_in_session(pgid, sid) {
        return true; // no members at all — effectively orphaned
    }

    // Assume orphaned; one member with a parent in a different pgrp of the same
    // session disproves it. The visitor resolves that parent through the
    // registry, which is why iteration hands out its guards off-lock.
    let mut is_orphaned = true;
    task::task_try_for_each_active(|task| {
        // Only look at members of the target process group.
        if task.pgid != pgid || task.sid != sid {
            return ControlFlow::Continue(());
        }

        let parent_id = task.parent_task_id;
        if parent_id == 0 || parent_id == slopos_abi::task::INVALID_TASK_ID {
            return ControlFlow::Continue(()); // no parent or init — can't help
        }

        let Some(parent) = task::task_find_by_id(parent_id) else {
            return ControlFlow::Continue(());
        };

        // Parent is in the same session but a different pgrp → not orphaned.
        if parent.sid == sid && parent.pgid != pgid {
            is_orphaned = false;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });

    is_orphaned
}

// ---------------------------------------------------------------------------
// Service table — pure forwards reference the real function directly.
// ---------------------------------------------------------------------------

static DRIVER_RUNTIME_SERVICES: DriverRuntimeServices = DriverRuntimeServices {
    save_preempt_context: scheduler::save_preempt_context,
    scheduler_timer_tick: scheduler::scheduler_timer_tick,
    scheduler_handle_timer_interrupt: scheduler::scheduler_handle_timer_interrupt,
    request_reschedule_from_interrupt: scheduler::scheduler_request_reschedule_from_interrupt,
    scheduler_is_enabled: scheduler::scheduler_is_enabled,
    current_task_id: scheduler::current_task_id,
    current_task_handle: scheduler::current_task_handle,
    current_task_pgid: scheduler::current_task_pgid,
    current_task_sid: scheduler::current_task_sid,
    current_task_controlling_tty: scheduler::current_task_controlling_tty,
    set_current_task_controlling_tty: scheduler::set_current_task_controlling_tty,
    clear_session_controlling_tty: scheduler::clear_session_controlling_tty,
    block_current_task_with_timeout: scheduler::block_current_task_with_timeout,
    sleep_current_task_ms: scheduler::sleep_current_task_ms,
    mark_current_blocked: scheduler::mark_current_blocked,
    yield_blocked_task: scheduler::yield_blocked_task,
    yield_blocked_task_with_timeout: scheduler::yield_blocked_task_with_timeout,
    set_current_runnable: scheduler::set_current_runnable,
    unblock_task: runtime_unblock_task,
    register_idle_wakeup_callback: scheduler::scheduler_register_idle_wakeup_callback,
    signal_process_group: runtime_signal_process_group,
    signal_session: runtime_signal_session,
    pgrp_handle: slopos_sched::task::pgrp_handle_for_pgid,
    session_handle: slopos_sched::task::session_handle_for_sid,
    current_task_pgrp_handle: runtime_current_task_pgrp_handle,
    pgrp_exists_in_session: runtime_pgrp_exists_in_session,
    is_current_signal_blocked_or_ignored: runtime_is_current_signal_blocked_or_ignored,
    is_pgrp_orphaned: runtime_is_pgrp_orphaned,
    has_pending_signal: runtime_has_pending_signal,
    debug_dump_tasks: slopos_sched::task::debug_dump_tasks_klog,
    irq_init: irq::init,
    irq_set_route: irq::set_irq_route,
    irq_is_masked: irq::is_masked,
    irq_enable_line: irq::enable_line,
    irq_disable_line: irq::disable_line,
    irq_get_timer_ticks: irq::get_timer_ticks,
    irq_increment_timer_ticks: irq::increment_timer_ticks,
    irq_increment_keyboard_events: irq::increment_keyboard_events,
};

pub fn register_driver_services() {
    register_driver_runtime_services(&DRIVER_RUNTIME_SERVICES);
}
