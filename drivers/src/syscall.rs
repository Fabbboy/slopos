use slopos_lib::klog_info;

use crate::syscall_handlers::syscall_lookup;
use crate::syscall_types::{InterruptFrame, TASK_FLAG_NO_PREEMPT, TASK_FLAG_USER_MODE, Task, TaskContext};
use crate::{scheduler_callbacks, wl_currency};

const GDT_USER_DATA_SELECTOR: u64 = 0x1B;

fn save_user_context(frame: *mut InterruptFrame, task: *mut Task) {
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
        ctx.ds = GDT_USER_DATA_SELECTOR as u64;
        ctx.es = GDT_USER_DATA_SELECTOR as u64;
        ctx.fs = 0;
        ctx.gs = 0;

        (*task).context_from_user = 1;
        (*task).user_started = 1;
    }
}
pub fn syscall_handle(frame: *mut InterruptFrame) {
    if frame.is_null() {
        wl_currency::award_loss();
        return;
    }

    let task = unsafe { scheduler_callbacks::call_get_current_task() as *mut Task };
    unsafe {
        if task.is_null() || ((*task).flags & TASK_FLAG_USER_MODE) == 0 {
            wl_currency::award_loss();
            return;
        }
    }

    save_user_context(frame, task);
    unsafe {
        (*task).flags |= TASK_FLAG_NO_PREEMPT;
    }

    // Temporarily set current task provider to use this task's process_id
    // This ensures user_copy_from_user can find the correct page directory
    let pid = unsafe { (*task).process_id };
    let original_provider = slopos_mm::user_copy::set_syscall_process_id(pid);

    let sysno = unsafe { (*frame).rax };
    let entry = syscall_lookup(sysno);
    if entry.is_null() {
        klog_info!("SYSCALL: Unknown syscall {}", sysno);
        unsafe {
            wl_currency::award_loss();
            (*frame).rax = u64::MAX;
        }
        unsafe {
            (*task).flags &= !TASK_FLAG_NO_PREEMPT;
        }
        slopos_mm::user_copy::restore_task_provider(original_provider);
        return;
    }

    let handler = unsafe { (*entry).handler };
    if let Some(func) = handler {
        func(task, frame);
    }

    unsafe {
        (*task).flags &= !TASK_FLAG_NO_PREEMPT;
    }
    slopos_mm::user_copy::restore_task_provider(original_provider);
}
