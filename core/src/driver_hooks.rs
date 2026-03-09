use core::ffi::c_void;
use core::sync::atomic::Ordering;

use slopos_abi::signal::sig_bit;
use slopos_lib::kernel_services::driver_runtime::{
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

fn runtime_current_task_id() -> u32 {
    let task = scheduler::scheduler_get_current_task();
    if task.is_null() {
        return 0;
    }
    unsafe { (*task).task_id }
}

fn runtime_current_task_pgid() -> u32 {
    let task = scheduler::scheduler_get_current_task();
    if task.is_null() {
        return 0;
    }
    unsafe { (*task).pgid }
}

fn runtime_current_task_sid() -> u32 {
    let task = scheduler::scheduler_get_current_task();
    if task.is_null() {
        return 0;
    }
    unsafe { (*task).sid }
}

fn runtime_current_task_controlling_tty() -> Option<slopos_abi::syscall::TtyIndex> {
    let task = scheduler::scheduler_get_current_task();
    if task.is_null() {
        return None;
    }
    unsafe { (*task).controlling_tty }
}

fn runtime_set_current_task_controlling_tty(tty: Option<slopos_abi::syscall::TtyIndex>) -> bool {
    let task = scheduler::scheduler_get_current_task();
    if task.is_null() {
        return false;
    }
    unsafe {
        (*task).controlling_tty = tty;
    }
    true
}

fn runtime_clear_session_controlling_tty(
    session_id: u32,
    tty: slopos_abi::syscall::TtyIndex,
) -> usize {
    task::task_clear_controlling_tty_for_session(session_id, tty)
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
// Phase 24: Check if a process group exists within a given session.
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
// Service table — pure forwards reference the real function directly.
// ---------------------------------------------------------------------------

static DRIVER_RUNTIME_SERVICES: DriverRuntimeServices = DriverRuntimeServices {
    save_preempt_context: scheduler::save_preempt_context,
    scheduler_timer_tick: scheduler::scheduler_timer_tick,
    scheduler_handle_timer_interrupt: scheduler::scheduler_handle_timer_interrupt,
    request_reschedule_from_interrupt: scheduler::scheduler_request_reschedule_from_interrupt,
    scheduler_is_enabled: scheduler::scheduler_is_enabled,
    current_task: runtime_current_task,
    current_task_id: runtime_current_task_id,
    current_task_pgid: runtime_current_task_pgid,
    current_task_sid: runtime_current_task_sid,
    current_task_controlling_tty: runtime_current_task_controlling_tty,
    set_current_task_controlling_tty: runtime_set_current_task_controlling_tty,
    clear_session_controlling_tty: runtime_clear_session_controlling_tty,
    block_current_task: scheduler::block_current_task,
    unblock_task: runtime_unblock_task,
    register_idle_wakeup_callback: scheduler::scheduler_register_idle_wakeup_callback,
    signal_process_group: runtime_signal_process_group,
    signal_session: runtime_signal_session,
    pgrp_exists_in_session: runtime_pgrp_exists_in_session,
    irq_init: irq::init,
    irq_set_route: irq::set_irq_route,
    irq_is_masked: irq::is_masked,
    irq_register_handler: irq::register_handler,
    irq_enable_line: irq::enable_line,
    irq_disable_line: irq::disable_line,
    irq_get_timer_ticks: irq::get_timer_ticks,
    irq_increment_timer_ticks: irq::increment_timer_ticks,
    irq_increment_keyboard_events: irq::increment_keyboard_events,
};

pub fn register_driver_services() {
    register_driver_runtime_services(&DRIVER_RUNTIME_SERVICES);
}
