use slopos_arch::arch::gdt::SegmentSelector;
use slopos_arch::{InterruptFrame, MAX_CPUS, cpu};
use slopos_mm::memory_layout_defs::{EXCEPTION_STACK_REGION_BASE, EXCEPTION_STACK_REGION_STRIDE};
use slopos_sync::preempt::PreemptGuard;

use super::scheduler::{
    is_scheduling_active, schedule_from_trap_exit, scheduler_get_current_task, scheduler_timer_tick,
};
use super::task::{TASK_FLAG_USER_MODE, Task, TaskContext};

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
fn trap_running_on_exception_stack() -> bool {
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
    if frame.is_null() || task.is_null() {
        return;
    }

    unsafe {
        let ctx: &mut TaskContext = &mut (*task).context;
        ctx.rax = (*frame).rax;
        ctx.rbx = (*frame).rbx;
        ctx.rcx = (*frame).rcx;
        ctx.rdx = (*frame).rdx;
        ctx.rsi = (*frame).rsi;
        ctx.rdi = (*frame).rdi;
        ctx.rbp = (*frame).rbp;
        ctx.r8 = (*frame).r8;
        ctx.r9 = (*frame).r9;
        ctx.r10 = (*frame).r10;
        ctx.r11 = (*frame).r11;
        ctx.r12 = (*frame).r12;
        ctx.r13 = (*frame).r13;
        ctx.r14 = (*frame).r14;
        ctx.r15 = (*frame).r15;
        ctx.rip = (*frame).rip;
        ctx.rsp = (*frame).rsp;
        ctx.rflags = (*frame).rflags;
        ctx.cs = (*frame).cs;
        ctx.ss = (*frame).ss;
        ctx.ds = SegmentSelector::USER_DATA.bits() as u64;
        ctx.es = SegmentSelector::USER_DATA.bits() as u64;
        ctx.fs = 0;
        ctx.gs = 0;

        (*task).context_from_user = 1;
        if mark_user_started {
            (*task).user_started = 1;
        }
    }
}

pub fn scheduler_request_reschedule(_reason: RescheduleReason) {
    if is_scheduling_active() {
        PreemptGuard::set_reschedule_pending();
    }
}

pub fn scheduler_request_reschedule_from_interrupt() {
    scheduler_request_reschedule(RescheduleReason::InterruptWake);
}

pub fn scheduler_handle_timer_interrupt(_frame: *mut InterruptFrame) {
    scheduler_timer_tick();
}

pub fn save_preempt_context(frame: *mut InterruptFrame) {
    if frame.is_null() {
        return;
    }

    let task = scheduler_get_current_task();
    if task.is_null() {
        return;
    }

    let is_user_mode = unsafe { (*task).flags & TASK_FLAG_USER_MODE != 0 };
    if !is_user_mode {
        return;
    }

    let cs = unsafe { (*frame).cs };
    if (cs & 3) != 3 {
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

    if is_scheduling_active() {
        PreemptGuard::clear_reschedule_pending();
        schedule_from_trap_exit();
    }
}

pub fn scheduler_handle_post_irq() {
    scheduler_handoff_on_trap_exit(TrapExitSource::Irq);
}
