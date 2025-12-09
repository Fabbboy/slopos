#![allow(dead_code)]
#![allow(non_camel_case_types)]

use core::ffi::c_char;

use slopos_lib::klog_printf;
use slopos_lib::KlogLevel;

use crate::syscall_handlers::syscall_lookup;
use crate::syscall_types::{task_t, task_context_t, InterruptFrame, TASK_FLAG_USER_MODE};
use crate::wl_currency;

const GDT_USER_DATA_SELECTOR: u64 = 0x1B;

extern "C" {
    fn scheduler_get_current_task() -> *mut task_t;
}

fn save_user_context(frame: *mut InterruptFrame, task: *mut task_t) {
    if frame.is_null() || task.is_null() {
        return;
    }

    unsafe {
        let ctx: &mut task_context_t = &mut (*task).context;
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

#[no_mangle]
pub extern "C" fn syscall_handle(frame: *mut InterruptFrame) {
    if frame.is_null() {
        wl_currency::award_loss();
        return;
    }

    let task = unsafe { scheduler_get_current_task() };
    unsafe {
        if task.is_null() || ((*task).flags & TASK_FLAG_USER_MODE) == 0 {
            wl_currency::award_loss();
            return;
        }
    }

    save_user_context(frame, task);

    let sysno = unsafe { (*frame).rax };
    let entry = syscall_lookup(sysno);
    if entry.is_null() {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"SYSCALL: Unknown syscall %llu\n\0".as_ptr() as *const c_char,
                sysno,
            );
            wl_currency::award_loss();
            (*frame).rax = u64::MAX;
        }
        return;
    }

    let handler = unsafe { (*entry).handler };
    if let Some(func) = handler {
        func(task, frame);
    }
}

