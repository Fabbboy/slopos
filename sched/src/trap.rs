use slopos_arch::{InterruptFrame, MAX_CPUS, cpu};
use slopos_mm::memory_layout_defs::{EXCEPTION_STACK_REGION_BASE, EXCEPTION_STACK_REGION_STRIDE};
use slopos_ostd::sync::PreemptGuard;

use super::scheduler::{is_scheduling_active, schedule_from_trap_exit, scheduler_timer_tick};
use super::task::{TASK_FLAG_USER_MODE, TaskStatus};

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

/// Save the interrupted user-mode frame into the running task's context. The
/// `Current` guard witnesses that the task written is the one this CPU runs.
pub fn save_task_context_from_interrupt_frame(
    current: &crate::task_struct::Current,
    frame: &InterruptFrame,
    mark_user_started: bool,
) {
    current
        .task()
        .save_from_interrupt_frame(current, frame, mark_user_started);
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
    // Ahead of `scheduler_timer_tick` so the heartbeat is recorded before any
    // lock is taken, and outside it so synthetic test calls to that function do
    // not drive the detector with no wall time elapsed.
    slopos_ostd::watchdog::tick();
    slopos_mm::mmu::quiesce::tick();
    slopos_ostd::kconsole::poll_from_timer();
    save_preempt_context(frame);
    scheduler_timer_tick();
}

pub fn save_preempt_context(frame: *mut InterruptFrame) {
    // The frame lives for exactly this handler invocation, so a frame-local is
    // the honest anchor for a borrow of it.
    let frame_anchor = ();
    let Some(frame_ref) = InterruptFrame::from_ptr(&frame_anchor, frame) else {
        return;
    };

    let Some(current) = crate::task_struct::Current::get() else {
        return;
    };
    if current.task().flags & TASK_FLAG_USER_MODE == 0 {
        return;
    }
    if (frame_ref.cs & 3) != 3 {
        return;
    }

    save_task_context_from_interrupt_frame(&current, frame_ref, false);
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

    // A task that has committed `Running → Blocked` but not yet yielded must
    // not be descheduled from the trap exit: it would park with no wake armed
    // and no timeout.
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
