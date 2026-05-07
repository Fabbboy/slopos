use core::ffi::{c_char, c_void};
use core::mem::size_of;

use slopos_abi::syscall::{ERRNO_EINVAL, TtyIndex, UserSysInfo};
use slopos_abi::task::{TaskExitReason, TaskFaultReason};
use slopos_abi::tty_error::TtyError;
use slopos_abi::{USER_NET_MAX_MEMBERS, UserNetInfo, UserNetMember};
use slopos_ostd::user::context::UserContext;
use slopos_utils::klog_debug;

use crate::sched::{
    get_scheduler_stats, schedule, scheduler_is_preemption_enabled, sleep_current_task_ms, yield_,
};
use crate::scheduler::task_struct::Task;
use crate::syscall::common::{
    SyscallDisposition, USER_IO_MAX_BYTES, syscall_bounded_from_user, syscall_copy_to_user_bounded,
    syscall_return_err,
};
use crate::syscall::context::SyscallContext;
use crate::task::{get_task_stats, task_terminate};
use slopos_kernel_services::platform;
use slopos_kernel_services::syscall_services::tty;

use slopos_mm::page_alloc::get_page_allocator_stats;
use slopos_mm::user_copy::copy_to_user;
use slopos_mm::user_ptr::UserPtr;

pub fn syscall_yield(task: *mut Task, ctx_ptr: *mut UserContext) -> SyscallDisposition {
    let Some(user_ctx) = UserContext::from_ptr_mut(ctx_ptr) else {
        return syscall_return_err(ctx_ptr, ERRNO_EINVAL);
    };
    let Some(ctx) = SyscallContext::from_user_context(task, user_ctx) else {
        return syscall_return_err(ctx_ptr, ERRNO_EINVAL);
    };
    let _ = ctx.ok(0);
    yield_();
    SyscallDisposition::Ok
}

define_syscall!(syscall_get_time_ms(ctx, args) {
    let _ = args;
    let ms = slopos_utils::clock::uptime_ms();
    ctx.ok(ms)
});

define_syscall!(syscall_clock_gettime(ctx, args) {
    use slopos_abi::syscall::{CLOCK_MONOTONIC, CLOCK_REALTIME, Timespec};

    let clock_id = args.arg0;
    if clock_id != CLOCK_MONOTONIC && clock_id != CLOCK_REALTIME {
        return ctx.err();
    }

    require_nonzero!(ctx, args.arg1);

    let ns = slopos_utils::clock::monotonic_ns();
    let ts = Timespec {
        tv_sec: ns / 1_000_000_000,
        tv_nsec: ns % 1_000_000_000,
    };

    let user_ptr = try_or_err!(ctx, UserPtr::<Timespec>::try_new(args.arg1));
    try_or_err!(ctx, copy_to_user(user_ptr, &ts));
    ctx.ok(0)
});

pub fn syscall_halt(_task: *mut Task, _ctx_ptr: *mut UserContext) -> SyscallDisposition {
    platform::kernel_shutdown(b"user halt\0".as_ptr() as *const c_char);
    #[allow(unreachable_code)]
    SyscallDisposition::Ok
}

pub fn syscall_reboot(_task: *mut Task, _ctx_ptr: *mut UserContext) -> SyscallDisposition {
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
        slopos_kernel_services::platform::timer_poll_delay_ms(ms as u32);
        0
    };
    if rc == 0 {
        ctx.ok(0)
    } else {
        ctx.err()
    }
});

pub fn syscall_exit(task: *mut Task, ctx_ptr: *mut UserContext) -> SyscallDisposition {
    let ctx = match UserContext::from_ptr_mut(ctx_ptr) {
        Some(uc) => SyscallContext::from_user_context(task, uc),
        None => None,
    };
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
    let n = match read_len {
        Ok(n) => n,
        Err(TtyError::Restart) => {
            return ctx.err_with(slopos_abi::syscall::ERRNO_ERESTARTSYS);
        }
        Err(_) => return ctx.err(),
    };

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
        wl_balance: slopos_utils::wl_currency::check_balance(),
        boot_flags: slopos_utils::boot_flags::get_flags(),
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

    let mut scratch = [UserNetMember::default(); USER_NET_MAX_MEMBERS];
    let discovered =
        slopos_net::netinfo::net_scan_members(&mut scratch[..max_members], args.arg2 != 0)
            .min(max_members)
            .min(USER_NET_MAX_MEMBERS);

    let mut i = 0usize;
    while i < discovered {
        let dst = args.arg0.wrapping_add((i * size_of::<UserNetMember>()) as u64);
        let user_ptr = try_or_err!(ctx, UserPtr::<UserNetMember>::try_new(dst));
        try_or_err!(ctx, copy_to_user(user_ptr, &scratch[i]));
        i += 1;
    }

    slopos_utils::wl_currency::adjust_balance(slopos_utils::wl_currency::WL_DELTA);
    ctx.ok(discovered as u64)
});

define_syscall!(syscall_net_info(ctx, args) {
    require_nonzero!(ctx, args.arg0);

    let ready = slopos_net::netinfo::net_is_ready();
    let mut info = UserNetInfo::default();
    info.nic_ready = u8::from(ready);

    if ready {
        let _ = slopos_net::netinfo::net_get_info(&mut info);
    }

    let user_ptr = try_or_err!(ctx, UserPtr::<UserNetInfo>::try_new(args.arg0));
    try_or_err!(ctx, copy_to_user(user_ptr, &info));
    ctx.ok(0)
});

define_syscall!(syscall_process_list(ctx, args) {
    require_nonzero!(ctx, args.arg0);
    require_nonzero!(ctx, args.arg1);

    use slopos_abi::syscall::UserTaskEntry;
    use slopos_abi::task::{INVALID_TASK_ID, MAX_TASKS};
    use slopos_ostd::KVec;
    use crate::task::task_iterate_active;
    // Allocate exactly `max_entries` (caller-requested, bounded by
    // `MAX_TASKS`) — not `MAX_TASKS` unconditionally; scanning 8192
    // default-initialised entries per syscall is unacceptable overhead
    // when the caller only wants the first few.

    // `IterCtx` holds the entries on the heap via `KVec` rather than
    // inline — a `[UserTaskEntry; MAX_TASKS]` array would push this
    // syscall's frame past the 2 KiB stack-gate at the current
    // `MAX_TASKS` value.
    struct IterCtx {
        entries: KVec<UserTaskEntry>,
        count: usize,
        max: usize,
    }

    fn collect_task(task_ptr: *mut crate::scheduler::task_struct::Task, ctx_ptr: *mut c_void) {
        // SAFETY: collect_task is only invoked through `iterate_tasks`,
        // which passes back the same `*mut IterCtx` the caller stashed
        // a moment ago.
        let mut cb = unsafe { slopos_ostd::util::callback_ctx::CallbackCtx::<IterCtx>::from_raw(ctx_ptr) };
        let Some(iter_ctx) = cb.try_borrow() else {
            return;
        };
        if iter_ctx.count >= iter_ctx.max {
            return;
        }

        let Some(task) = crate::scheduler::task::task_borrow(task_ptr) else {
            return;
        };
        if task.task_id == INVALID_TASK_ID {
            return;
        }

        let entry = &mut iter_ctx.entries[iter_ctx.count];
        entry.task_id = task.task_id;
        entry.parent_task_id = task.parent_task_id;
        entry.process_id = task.process_id;
        entry.state = task.status().as_u8();
        entry.block_reason = task.load_block_reason().as_u8();
        entry.priority = task.priority.as_u8();
        entry.last_cpu = task.last_cpu;
        entry.cpu_affinity = task.cpu_affinity;
        entry.total_runtime_us = slopos_utils::clock::ticks_to_microseconds(task.total_runtime);
        entry.creation_time_ms = task.creation_time;
        entry.yield_count = task.yield_count;
        entry.name = task.name;
        iter_ctx.count += 1;
    }

    let max_entries = (args.arg1 as usize).min(MAX_TASKS);
    let entries = match KVec::<UserTaskEntry>::with_capacity(max_entries) {
        Ok(mut v) => {
            for _ in 0..max_entries {
                if v.push(UserTaskEntry::default()).is_err() {
                    return ctx.err();
                }
            }
            v
        }
        Err(_) => return ctx.err(),
    };
    let mut iter_ctx = IterCtx {
        entries,
        count: 0,
        max: max_entries,
    };

    task_iterate_active(Some(collect_task), (&mut iter_ctx as *mut IterCtx).cast());

    let count = iter_ctx.count;
    for i in 0..count {
        let dst_addr = args
            .arg0
            .wrapping_add((i * core::mem::size_of::<UserTaskEntry>()) as u64);
        let user_ptr = try_or_err!(ctx, UserPtr::<UserTaskEntry>::try_new(dst_addr));
        try_or_err!(ctx, copy_to_user(user_ptr, &iter_ctx.entries[i]));
    }

    ctx.ok(count as u64)
});

define_syscall!(syscall_cpu_info(ctx, args) {
    require_nonzero!(ctx, args.arg0);

    use slopos_abi::syscall::UserCpuInfo;
    use slopos_arch::cpu::cpuid;

    let mut info = UserCpuInfo::default();
    info.vendor = cpuid::cpu_vendor_string();
    info.brand_string = cpuid::cpu_brand_string();
    info.cpu_count = slopos_arch::pcr::get_cpu_count() as u32;
    let (family, model, stepping) = cpuid::cpu_family_model_stepping();
    info.family = family;
    info.model = model;
    info.stepping = stepping;
    info.features = cpuid::cpu_features_bitmask();

    let user_ptr = try_or_err!(ctx, UserPtr::<UserCpuInfo>::try_new(args.arg0));
    try_or_err!(ctx, copy_to_user(user_ptr, &info));
    ctx.ok(0)
});

define_syscall!(syscall_percpu_stats(ctx, args) {
    require_nonzero!(ctx, args.arg0);
    require_nonzero!(ctx, args.arg1);

    use core::sync::atomic::Ordering;
    use slopos_abi::syscall::UserPerCpuStats;

    let cpu_count = slopos_arch::pcr::get_cpu_count();
    let max_entries = (args.arg1 as usize).min(cpu_count);

    for i in 0..max_entries {
        let stats = crate::per_cpu::with_cpu_scheduler(i, |sched| UserPerCpuStats {
            cpu_id: i as u32,
            _pad: 0,
            total_switches: sched.total_switches.load(Ordering::Relaxed),
            total_ticks: sched.total_ticks.load(Ordering::Relaxed),
            idle_ticks: sched.idle_time.load(Ordering::Relaxed),
            ready_count: sched.total_ready_count(),
            _pad2: 0,
        })
        .unwrap_or(UserPerCpuStats {
            cpu_id: i as u32,
            ..UserPerCpuStats::default()
        });

        let dst_addr = args
            .arg0
            .wrapping_add((i * core::mem::size_of::<UserPerCpuStats>()) as u64);
        let user_ptr = try_or_err!(ctx, UserPtr::<UserPerCpuStats>::try_new(dst_addr));
        try_or_err!(ctx, copy_to_user(user_ptr, &stats));
    }

    ctx.ok(max_entries as u64)
});
