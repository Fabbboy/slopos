use core::ffi::c_char;
use core::mem::size_of;

use slopos_abi::syscall::{TtyIndex, UserSysInfo, ERRNO_EINVAL};
use slopos_abi::task::{TaskExitReason, TaskFaultReason};
use slopos_abi::{UserNetInfo, UserNetMember, USER_NET_MAX_MEMBERS};
use slopos_lib::{klog_debug, InterruptFrame};

use crate::platform;
use crate::sched::{
    clear_scheduler_current_task, get_scheduler_stats, schedule, scheduler_is_preemption_enabled,
    sleep_current_task_ms, yield_,
};
use crate::scheduler::task_struct::Task;
use crate::syscall::common::{
    syscall_bounded_from_user, syscall_copy_to_user_bounded, syscall_return_err,
    SyscallDisposition, USER_IO_MAX_BYTES,
};
use crate::syscall::context::SyscallContext;
use crate::task::{get_task_stats, task_terminate};
use slopos_lib::kernel_services::syscall_services::{net, tty};

use slopos_mm::page_alloc::get_page_allocator_stats;
use slopos_mm::user_copy::copy_to_user;
use slopos_mm::user_ptr::UserPtr;

pub fn syscall_yield(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, ERRNO_EINVAL);
    };
    let _ = ctx.ok(0);
    yield_();
    SyscallDisposition::Ok
}

define_syscall!(syscall_get_time_ms(ctx, args) {
    let _ = args;
    let ms = slopos_lib::clock::uptime_ms();
    ctx.ok(ms)
});

define_syscall!(syscall_clock_gettime(ctx, args) {
    use slopos_abi::syscall::{CLOCK_MONOTONIC, Timespec};

    let clock_id = args.arg0;
    if clock_id != CLOCK_MONOTONIC {
        return ctx.err();
    }

    require_nonzero!(ctx, args.arg1);

    let ns = slopos_lib::clock::monotonic_ns();
    let ts = Timespec {
        tv_sec: ns / 1_000_000_000,
        tv_nsec: ns % 1_000_000_000,
    };

    let user_ptr = try_or_err!(ctx, UserPtr::<Timespec>::try_new(args.arg1));
    try_or_err!(ctx, copy_to_user(user_ptr, &ts));
    ctx.ok(0)
});

pub fn syscall_halt(_task: *mut Task, _frame: *mut InterruptFrame) -> SyscallDisposition {
    platform::kernel_shutdown(b"user halt\0".as_ptr() as *const c_char);
    #[allow(unreachable_code)]
    SyscallDisposition::Ok
}

pub fn syscall_reboot(_task: *mut Task, _frame: *mut InterruptFrame) -> SyscallDisposition {
    platform::kernel_reboot(b"user reboot\0".as_ptr() as *const c_char);
    #[allow(unreachable_code)]
    SyscallDisposition::Ok
}

define_syscall!(syscall_sleep_ms(ctx, args) {
    let mut ms = args.arg0;
    if ms > 60000 {
        ms = 60000;
    }
    let rc = if scheduler_is_preemption_enabled() != 0 {
        sleep_current_task_ms(ms as u32)
    } else {
        crate::platform::timer_poll_delay_ms(ms as u32);
        0
    };
    if rc == 0 {
        ctx.ok(0)
    } else {
        ctx.err()
    }
});

pub fn syscall_exit(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let ctx = SyscallContext::new(task, frame);
    let task_id = ctx.as_ref().and_then(|c| c.task_id()).unwrap_or(u32::MAX);
    klog_debug!("SYSCALL_EXIT: task {} entering exit", task_id);
    if let Some(ref c) = ctx {
        let code = c.args().arg0 as u32;
        if let Some(t) = c.task_mut() {
            t.exit_reason = TaskExitReason::Normal;
            t.fault_reason = TaskFaultReason::None;
            t.exit_code = code;
        }
    }
    klog_debug!("SYSCALL_EXIT: task {} calling task_terminate", task_id);
    task_terminate(task_id);
    clear_scheduler_current_task();
    schedule();
    klog_debug!(
        "SYSCALL_EXIT: task {} schedule returned (should not happen)",
        task_id
    );
    SyscallDisposition::NoReturn
}

define_syscall!(syscall_user_write(ctx, args) {
    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    require_nonzero!(ctx, args.arg0);
    let write_len = try_or_err!(ctx, syscall_bounded_from_user(&mut tmp, args.arg0, args.arg1, USER_IO_MAX_BYTES));
    platform::console_puts(&tmp[..write_len]);
    ctx.ok(write_len as u64)
});

define_syscall!(syscall_user_read(ctx, args) {
    require_nonzero!(ctx, args.arg0);
    require_nonzero!(ctx, args.arg1);

    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let max_len = args.arg1_usize().min(USER_IO_MAX_BYTES);

    let read_len = tty::read_cooked(TtyIndex(0), tmp.as_mut_ptr(), max_len, false);
    if read_len < 0 {
        // Propagate ERESTARTSYS directly.
        if read_len == -512 {
            return ctx.err_with(slopos_abi::syscall::ERRNO_ERESTARTSYS);
        }
        return ctx.err();
    }

    let n = read_len as usize;
    try_or_err!(ctx, syscall_copy_to_user_bounded(args.arg0, &tmp[..n]));
    ctx.ok(n as u64)
});

define_syscall!(syscall_sys_info(ctx, args) {
    require_nonzero!(ctx, args.arg0);

    let mut info = UserSysInfo {
        total_pages: 0,
        free_pages: 0,
        allocated_pages: 0,
        total_tasks: 0,
        active_tasks: 0,
        task_context_switches: 0,
        scheduler_context_switches: 0,
        scheduler_yields: 0,
        ready_tasks: 0,
        schedule_calls: 0,
        wl_balance: slopos_lib::wl_currency::check_balance(),
        boot_flags: slopos_lib::boot_flags::get_flags(),
    };

    get_page_allocator_stats(
        &mut info.total_pages,
        &mut info.free_pages,
        &mut info.allocated_pages,
    );
    get_task_stats(
        &mut info.total_tasks,
        &mut info.active_tasks,
        &mut info.task_context_switches,
    );
    get_scheduler_stats(
        &mut info.scheduler_context_switches,
        &mut info.scheduler_yields,
        &mut info.ready_tasks,
        &mut info.schedule_calls,
    );

    let user_ptr = try_or_err!(ctx, UserPtr::<UserSysInfo>::try_new(args.arg0));
    try_or_err!(ctx, copy_to_user(user_ptr, &info));
    ctx.ok(0)
});

define_syscall!(syscall_net_scan(ctx, args) {
    require_nonzero!(ctx, args.arg0);

    let max_members = (args.arg1 as usize).min(USER_NET_MAX_MEMBERS);
    if max_members == 0 {
        return ctx.ok(0);
    }

    let active_probe: u32 = if args.arg2 != 0 { 1 } else { 0 };
    let mut scratch = [UserNetMember::default(); USER_NET_MAX_MEMBERS];
    let discovered = net::scan_members(scratch.as_mut_ptr(), max_members, active_probe)
        .min(max_members)
        .min(USER_NET_MAX_MEMBERS);

    let mut i = 0usize;
    while i < discovered {
        let dst = args.arg0.wrapping_add((i * size_of::<UserNetMember>()) as u64);
        let user_ptr = try_or_err!(ctx, UserPtr::<UserNetMember>::try_new(dst));
        try_or_err!(ctx, copy_to_user(user_ptr, &scratch[i]));
        i += 1;
    }

    slopos_lib::wl_currency::adjust_balance(slopos_lib::wl_currency::WL_DELTA);
    ctx.ok(discovered as u64)
});

define_syscall!(syscall_net_info(ctx, args) {
    require_nonzero!(ctx, args.arg0);

    let ready = net::is_ready();
    let mut info = UserNetInfo::default();
    info.nic_ready = if ready != 0 { 1 } else { 0 };

    if ready != 0 {
        let _ = net::get_info(&mut info as *mut UserNetInfo);
    }

    let user_ptr = try_or_err!(ctx, UserPtr::<UserNetInfo>::try_new(args.arg0));
    try_or_err!(ctx, copy_to_user(user_ptr, &info));
    ctx.ok(0)
});
