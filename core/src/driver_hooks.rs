use core::ffi::c_void;
use core::sync::atomic::Ordering;

use slopos_abi::signal::{SIG_IGN, sig_bit};
use slopos_kernel_services::driver_runtime::{
    DriverRuntimeServices, DriverTaskHandle, register_driver_runtime_services,
};

use crate::irq;
use crate::scheduler::scheduler;
use crate::scheduler::task::{self, Task};

// ---------------------------------------------------------------------------
// Adapter functions — only for service methods that need type conversion or
// non-trivial logic.  Pure 1:1 forwards are assigned directly in the static
// service table below.
// ---------------------------------------------------------------------------

fn handle_to_task(handle: DriverTaskHandle) -> *mut Task {
    handle.cast::<Task>()
}

fn runtime_current_task() -> DriverTaskHandle {
    scheduler::scheduler_get_current_task().cast()
}

fn runtime_unblock_task(task: DriverTaskHandle) -> i32 {
    scheduler::unblock_task(handle_to_task(task))
}

struct SignalGroupContext {
    pgid: u32,
    signum: u8,
    matched: bool,
}

fn signal_group_task(task: *mut Task, context: *mut c_void) {
    if task.is_null() || context.is_null() {
        return;
    }

    let ctx = unsafe { &mut *context.cast::<SignalGroupContext>() };
    if unsafe { (*task).pgid } != ctx.pgid {
        return;
    }

    unsafe {
        (*task)
            .signal_pending
            .fetch_or(sig_bit(ctx.signum), Ordering::AcqRel);
    }
    let _ = scheduler::unblock_task(task);
    ctx.matched = true;
}

fn runtime_signal_process_group(pgid: u32, signum: u8) -> bool {
    if pgid == 0 {
        return false;
    }

    let mut ctx = SignalGroupContext {
        pgid,
        signum,
        matched: false,
    };

    task::task_iterate_active(
        Some(signal_group_task),
        (&mut ctx as *mut SignalGroupContext).cast(),
    );

    ctx.matched
}

struct SignalSessionContext {
    sid: u32,
    signum: u8,
    matched: bool,
}

fn signal_session_task(task: *mut Task, context: *mut c_void) {
    if task.is_null() || context.is_null() {
        return;
    }

    let ctx = unsafe { &mut *context.cast::<SignalSessionContext>() };
    if unsafe { (*task).sid } != ctx.sid {
        return;
    }

    unsafe {
        (*task)
            .signal_pending
            .fetch_or(sig_bit(ctx.signum), Ordering::AcqRel);
    }
    let _ = scheduler::unblock_task(task);
    ctx.matched = true;
}

fn runtime_signal_session(sid: u32, signum: u8) -> bool {
    if sid == 0 {
        return false;
    }

    let mut ctx = SignalSessionContext {
        sid,
        signum,
        matched: false,
    };

    task::task_iterate_active(
        Some(signal_session_task),
        (&mut ctx as *mut SignalSessionContext).cast(),
    );

    ctx.matched
}

// ---------------------------------------------------------------------------
// Check if a process group exists within a given session.
// ---------------------------------------------------------------------------

struct PgrpExistsContext {
    pgid: u32,
    sid: u32,
    found: bool,
}

fn pgrp_exists_task(task: *mut Task, context: *mut c_void) {
    if task.is_null() || context.is_null() {
        return;
    }

    let ctx = unsafe { &mut *context.cast::<PgrpExistsContext>() };
    if ctx.found {
        return; // already found, skip remaining tasks
    }
    let t = unsafe { &*task };
    if t.pgid == ctx.pgid && t.sid == ctx.sid {
        ctx.found = true;
    }
}

fn runtime_pgrp_exists_in_session(pgid: u32, sid: u32) -> bool {
    if pgid == 0 || sid == 0 {
        return false;
    }

    let mut ctx = PgrpExistsContext {
        pgid,
        sid,
        found: false,
    };

    task::task_iterate_active(
        Some(pgrp_exists_task),
        (&mut ctx as *mut PgrpExistsContext).cast(),
    );

    ctx.found
}

// ---------------------------------------------------------------------------
// Check if the current task has a signal blocked or set to SIG_IGN.
// ---------------------------------------------------------------------------

fn runtime_is_current_signal_blocked_or_ignored(signum: u8) -> bool {
    let task = scheduler::scheduler_get_current_task();
    if task.is_null() {
        return false;
    }
    let bit = sig_bit(signum);
    if bit == 0 {
        return false; // invalid signal number
    }
    unsafe {
        // Check if signal is in the blocked mask.
        if ((*task).signal_blocked & bit) != 0 {
            return true;
        }
        // Check if the signal handler is SIG_IGN.
        let idx = (signum as usize).wrapping_sub(1);
        if idx < (*task).signal_actions.len() && (*task).signal_actions[idx].handler == SIG_IGN {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Check if the current task has any deliverable (pending
// and not blocked) signals.  Used by the TTY read path to detect signal
// interruption and return ERESTARTSYS.
// ---------------------------------------------------------------------------

fn runtime_has_pending_signal() -> bool {
    let task = scheduler::scheduler_get_current_task();
    if task.is_null() {
        return false;
    }
    unsafe {
        let pending = (*task).signal_pending.load(Ordering::Acquire);
        let deliverable = pending & !(*task).signal_blocked;
        deliverable != 0
    }
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

struct OrphanCheckContext {
    pgid: u32,
    sid: u32,
    is_orphaned: bool,
}

fn orphan_check_task(task: *mut Task, context: *mut c_void) {
    if task.is_null() || context.is_null() {
        return;
    }

    let ctx = unsafe { &mut *context.cast::<OrphanCheckContext>() };
    if !ctx.is_orphaned {
        return; // already found a non-orphan indicator, skip
    }

    let t = unsafe { &*task };
    // Only look at members of the target process group.
    if t.pgid != ctx.pgid || t.sid != ctx.sid {
        return;
    }

    // Check if this member's parent is in a different pgrp within the same
    // session — if so, the pgrp is NOT orphaned.
    let parent_id = t.parent_task_id;
    if parent_id == 0 || parent_id == slopos_abi::task::INVALID_TASK_ID {
        return; // no parent or init — can't help
    }

    let parent = task::task_find_by_id(parent_id);
    if parent.is_null() {
        return;
    }

    let parent_ref = unsafe { &*parent };
    // Parent is in the same session but a different pgrp → not orphaned.
    if parent_ref.sid == ctx.sid && parent_ref.pgid != ctx.pgid {
        ctx.is_orphaned = false;
    }
}

fn runtime_is_pgrp_orphaned(pgid: u32, sid: u32) -> bool {
    if pgid == 0 || sid == 0 {
        return false; // no valid group/session — not orphaned by definition
    }

    // First check that the pgrp even has members in the session.
    if !runtime_pgrp_exists_in_session(pgid, sid) {
        return true; // no members at all — effectively orphaned
    }

    let mut ctx = OrphanCheckContext {
        pgid,
        sid,
        is_orphaned: true, // assume orphaned, disprove by finding a qualifying parent
    };

    task::task_iterate_active(
        Some(orphan_check_task),
        (&mut ctx as *mut OrphanCheckContext).cast(),
    );

    ctx.is_orphaned
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
    current_task: runtime_current_task,
    current_task_id: scheduler::current_task_id,
    current_task_pgid: scheduler::current_task_pgid,
    current_task_sid: scheduler::current_task_sid,
    current_task_controlling_tty: scheduler::current_task_controlling_tty,
    set_current_task_controlling_tty: scheduler::set_current_task_controlling_tty,
    clear_session_controlling_tty: scheduler::clear_session_controlling_tty,
    block_current_task: scheduler::block_current_task,
    block_current_task_with_timeout: scheduler::block_current_task_with_timeout,
    sleep_current_task_ms: scheduler::sleep_current_task_ms,
    prepare_to_wait: scheduler::prepare_to_wait,
    finish_wait: scheduler::finish_wait,
    unblock_task: runtime_unblock_task,
    register_idle_wakeup_callback: scheduler::scheduler_register_idle_wakeup_callback,
    register_bottom_half: scheduler::scheduler_register_bottom_half,
    run_bottom_halves: scheduler::scheduler_run_bottom_halves,
    spawn_kernel_task: scheduler::spawn_kernel_task_from_driver,
    signal_process_group: runtime_signal_process_group,
    signal_session: runtime_signal_session,
    pgrp_exists_in_session: runtime_pgrp_exists_in_session,
    is_current_signal_blocked_or_ignored: runtime_is_current_signal_blocked_or_ignored,
    is_pgrp_orphaned: runtime_is_pgrp_orphaned,
    has_pending_signal: runtime_has_pending_signal,
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
