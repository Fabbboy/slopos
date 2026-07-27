use slopos_arch::{InterruptFrame, MAX_CPUS, cpu};
use slopos_mm::memory_layout_defs::{EXCEPTION_STACK_REGION_BASE, EXCEPTION_STACK_REGION_STRIDE};
use slopos_ostd::sync::PreemptGuard;

use super::scheduler::{is_scheduling_active, schedule_from_trap_exit, scheduler_timer_tick};
use super::task::{
    TASK_FLAG_USER_MODE, Task, TaskStatus, task_has_flag, task_save_from_interrupt_frame,
};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RescheduleReason {
    TimerTick,
    InterruptWake,
    RescheduleIpi,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TrapExitSource {
    Irq,
    RescheduleIpi,
}

#[inline]
pub fn trap_running_on_exception_stack() -> bool {
    let rsp = cpu::read_rsp();
    let ist_region_end =
        EXCEPTION_STACK_REGION_BASE + (MAX_CPUS as u64) * 7 * EXCEPTION_STACK_REGION_STRIDE;
    rsp >= EXCEPTION_STACK_REGION_BASE && rsp < ist_region_end
}

pub fn save_task_context_from_interrupt_frame(
    task: *mut Task,
    frame: *mut InterruptFrame,
    mark_user_started: bool,
) {
    task_save_from_interrupt_frame(task, frame as *const InterruptFrame, mark_user_started);
}

pub fn scheduler_request_reschedule(_reason: RescheduleReason) {
    if is_scheduling_active() {
        PreemptGuard::set_reschedule_pending();
    }
}

pub fn scheduler_request_reschedule_from_interrupt() {
    scheduler_request_reschedule(RescheduleReason::InterruptWake);
}

pub fn scheduler_handle_timer_interrupt(frame: *mut InterruptFrame) {
    save_preempt_context(frame);
    scheduler_timer_tick();
}

pub fn save_preempt_context(frame: *mut InterruptFrame) {
    if frame.is_null() {
        return;
    }

    let Some(current) = crate::task_struct::Current::get() else {
        return;
    };
    let task = current.as_ptr();

    if !task_has_flag(task, TASK_FLAG_USER_MODE) {
        return;
    }

    // OSTD's `borrow_ref` folds the one `unsafe` reborrow; the
    // caller-supplied frame lives on the ISR stack for the duration
    // of this call.
    let frame_ref: &InterruptFrame = slopos_ostd::util::ptr_buf::borrow_ref(frame);
    if (frame_ref.cs & 3) != 3 {
        return;
    }

    save_task_context_from_interrupt_frame(task, frame, false);
}

pub fn scheduler_handoff_on_trap_exit(source: TrapExitSource) {
    if matches!(source, TrapExitSource::Irq) && trap_running_on_exception_stack() {
        return;
    }

    if PreemptGuard::is_active() {
        return;
    }

    if !PreemptGuard::is_reschedule_pending() {
        return;
    }

    // SM_PREEMPT discipline, IRQ-exit flavour (see
    // `deferred_reschedule_callback` for the full rationale): an IRQ that
    // lands between a wait primitive's `Running → Blocked` commit and its
    // voluntary yield must not deschedule the task from the trap exit —
    // doing so parks it before the condition recheck ran or a timeout was
    // armed, losing any wake that fired in the gap. Leave the pending flag
    // set; the task's own `schedule()` follows within the protocol and a
    // later trap exit re-checks the flag once a Running task is current.
    if crate::task_struct::Current::get()
        .is_some_and(|current| current.task().status() == TaskStatus::Blocked)
    {
        return;
    }

    if is_scheduling_active() {
        PreemptGuard::clear_reschedule_pending();
        schedule_from_trap_exit();
    }
}

pub fn scheduler_handle_post_irq() {
    scheduler_handoff_on_trap_exit(TrapExitSource::Irq);
}
