use core::ffi::{c_char, c_void};
use core::mem::size_of;

use slopos_abi::Errno;
use slopos_abi::syscall::{CLOCK_MONOTONIC, CLOCK_REALTIME, Timespec, TtyIndex, UserSysInfo};
use slopos_abi::task::{TaskExitReason, TaskFaultReason};
use slopos_abi::tty_error::TtyError;
use slopos_abi::{USER_NET_MAX_MEMBERS, UserNetInfo, UserNetMember};
use slopos_ostd::klog_debug;

use crate::syscall::args::{UserBytes, UserPtr};
use crate::syscall::common::{
    USER_IO_MAX_BYTES, syscall_bounded_from_user, syscall_copy_to_user_bounded,
};
use crate::syscall::result::SyscallResult;
use slopos_kernel_services::platform;
use slopos_kernel_services::syscall_services::tty;
use slopos_sched::scheduler::{
    get_scheduler_stats, schedule, scheduler_is_preemption_enabled, sleep_current_task_ms, yield_,
};
use slopos_sched::task::{get_task_stats, task_terminate};

use slopos_mm::page_alloc::get_page_allocator_stats;
use slopos_mm::user_copy::copy_to_user;

define_syscall!(syscall_yield (ctx) -> SyscallResult {
    // Bump the WL balance before yielding so the post-yield path
    // doesn't double-account. yield_() suspends; when control
    // resumes here the dispatcher's normal `write_ok` will adjust
    // again, so we return NoReturn and write rax manually.
    ctx.write_ok(0);
    yield_();
    SyscallResult::NoReturn
});

define_syscall!(syscall_get_time_ms (ctx) -> u64 {
    slopos_kernel_services::clock::uptime_ms()
});

define_syscall!(syscall_clock_gettime
    (ctx, clock_id: u64, ts: UserPtr<Timespec>) -> Result<(), Errno>
{
    if clock_id != CLOCK_MONOTONIC && clock_id != CLOCK_REALTIME {
        return Err(Errno::EINVAL);
    }
    let ns = slopos_kernel_services::clock::monotonic_ns();
    let value = Timespec {
        tv_sec: ns / 1_000_000_000,
        tv_nsec: ns % 1_000_000_000,
    };
    copy_to_user(ts.inner(), &value).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_halt (ctx) -> SyscallResult {
    platform::kernel_shutdown(b"user halt\0".as_ptr() as *const c_char);
    #[allow(unreachable_code)]
    SyscallResult::NoReturn
});

define_syscall!(syscall_reboot (ctx) -> SyscallResult {
    platform::kernel_reboot(b"user reboot\0".as_ptr() as *const c_char);
    #[allow(unreachable_code)]
    SyscallResult::NoReturn
});

define_syscall!(syscall_sleep_ms (ctx, ms: u64) -> Result<(), Errno> {
    let mut ms = ms;
    if ms > 60000 {
        ms = 60000;
    }
    let rc = if scheduler_is_preemption_enabled() != 0 {
        sleep_current_task_ms(ms as u32)
    } else {
        slopos_kernel_services::platform::timer_poll_delay_ms(ms as u32);
        0
    };
    if rc == 0 { Ok(()) } else { Err(Errno::EINVAL) }
});

define_syscall!(syscall_exit (ctx, code: u32) -> SyscallResult {
    let task_id = ctx.task_id().unwrap_or(u32::MAX);
    klog_debug!("SYSCALL_EXIT: task {} entering exit", task_id);
    if let Some(t) = ctx.task_mut() {
        t.exit_reason = TaskExitReason::Normal;
        t.fault_reason = TaskFaultReason::None;
        t.exit_code = code;
    }
    klog_debug!("SYSCALL_EXIT: task {} calling task_terminate", task_id);
    task_terminate(task_id);
    schedule();
    klog_debug!(
        "SYSCALL_EXIT: task {} schedule returned (should not happen)",
        task_id
    );
    SyscallResult::NoReturn
});

define_syscall!(syscall_user_write (ctx, buf: UserBytes) -> Result<u64, Errno> {
    if buf.base_u64() == 0 {
        return Err(Errno::EFAULT);
    }
    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let write_len = syscall_bounded_from_user(
        &mut tmp,
        buf.base_u64(),
        buf.len() as u64,
        USER_IO_MAX_BYTES,
    )
    .map_err(|_| Errno::EFAULT)?;
    platform::console_puts(&tmp[..write_len]);
    Ok(write_len as u64)
});

define_syscall!(syscall_user_read (ctx, buf: UserBytes) -> Result<u64, Errno> {
    if buf.base_u64() == 0 || buf.len() == 0 {
        return Err(Errno::EFAULT);
    }
    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let max_len = buf.len().min(USER_IO_MAX_BYTES);
    let read_len = tty::read_cooked(TtyIndex(0), tmp.as_mut_ptr(), max_len, false);
    let n = match read_len {
        Ok(n) => n,
        Err(TtyError::Restart) => return Err(Errno::ERESTARTSYS),
        Err(_) => return Err(Errno::EINVAL),
    };
    syscall_copy_to_user_bounded(buf.base_u64(), &tmp[..n]).map_err(|_| Errno::EFAULT)?;
    Ok(n as u64)
});

define_syscall!(syscall_sys_info (ctx, info_out: UserPtr<UserSysInfo>) -> Result<(), Errno> {
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
        wl_balance: slopos_ostd::wl_currency::check_balance(),
        boot_flags: slopos_ostd::boot_flags::get_flags(),
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

    copy_to_user(info_out.inner(), &info).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_net_scan
    (ctx, buf: UserPtr<UserNetMember>, max: u64, refresh: u64) -> Result<u64, Errno>
{
    let max_members = (max as usize).min(USER_NET_MAX_MEMBERS);
    if max_members == 0 {
        return Ok(0);
    }

    let mut scratch = [UserNetMember::default(); USER_NET_MAX_MEMBERS];
    let discovered = slopos_net::netinfo::net_scan_members(&mut scratch[..max_members], refresh != 0)
        .min(max_members)
        .min(USER_NET_MAX_MEMBERS);

    let mut i = 0usize;
    while i < discovered {
        let dst = buf.as_u64().wrapping_add((i * size_of::<UserNetMember>()) as u64);
        let user_ptr = slopos_mm::user_ptr::UserPtr::<UserNetMember>::try_new(dst)
            .map_err(|_| Errno::EFAULT)?;
        copy_to_user(user_ptr, &scratch[i]).map_err(|_| Errno::EFAULT)?;
        i += 1;
    }

    slopos_ostd::wl_currency::adjust_balance(slopos_ostd::wl_currency::WL_DELTA);
    Ok(discovered as u64)
});

define_syscall!(syscall_net_info (ctx, info_out: UserPtr<UserNetInfo>) -> Result<(), Errno> {
    let ready = slopos_net::netinfo::net_is_ready();
    let mut info = UserNetInfo::default();
    info.nic_ready = u8::from(ready);

    if ready {
        let _ = slopos_net::netinfo::net_get_info(&mut info);
    }

    copy_to_user(info_out.inner(), &info).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_process_list
    (ctx, buf: UserPtr<slopos_abi::syscall::UserTaskEntry>, max: u64) -> Result<u64, Errno>
{
    use slopos_abi::syscall::UserTaskEntry;
    use slopos_abi::task::{INVALID_TASK_ID, MAX_TASKS};
    use slopos_ostd::KVec;
    use slopos_sched::task::task_iterate_active;

    struct IterCtx {
        entries: KVec<UserTaskEntry>,
        count: usize,
        max: usize,
    }

    fn collect_task(task_ptr: *mut slopos_sched::task_struct::Task, ctx_ptr: *mut c_void) {
        let Some(iter_ctx) = slopos_ostd::util::ptr_buf::try_void_ctx_mut::<IterCtx>(ctx_ptr)
        else {
            return;
        };
        if iter_ctx.count >= iter_ctx.max {
            return;
        }

        let Some(task) = slopos_sched::task::task_borrow(task_ptr) else {
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
        entry.last_cpu = task.last_cpu();
        entry.cpu_affinity = task.cpu_affinity;
        entry.total_runtime_us =
            slopos_kernel_services::clock::ticks_to_microseconds(task.total_runtime);
        entry.creation_time_ms = task.creation_time;
        entry.yield_count = task.yield_count;
        entry.name = task.name;
        iter_ctx.count += 1;
    }

    let max_entries = (max as usize).min(MAX_TASKS);
    let entries = match KVec::<UserTaskEntry>::with_capacity(max_entries) {
        Ok(mut v) => {
            for _ in 0..max_entries {
                if v.push(UserTaskEntry::default()).is_err() {
                    return Err(Errno::ENOMEM);
                }
            }
            v
        }
        Err(_) => return Err(Errno::ENOMEM),
    };
    let mut iter_ctx = IterCtx {
        entries,
        count: 0,
        max: max_entries,
    };

    task_iterate_active(Some(collect_task), (&mut iter_ctx as *mut IterCtx).cast());

    let count = iter_ctx.count;
    for i in 0..count {
        let dst_addr = buf
            .as_u64()
            .wrapping_add((i * core::mem::size_of::<UserTaskEntry>()) as u64);
        let user_ptr = slopos_mm::user_ptr::UserPtr::<UserTaskEntry>::try_new(dst_addr)
            .map_err(|_| Errno::EFAULT)?;
        copy_to_user(user_ptr, &iter_ctx.entries[i]).map_err(|_| Errno::EFAULT)?;
    }

    Ok(count as u64)
});

define_syscall!(syscall_cpu_info
    (ctx, info_out: UserPtr<slopos_abi::syscall::UserCpuInfo>) -> Result<(), Errno>
{
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

    copy_to_user(info_out.inner(), &info).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_percpu_stats
    (ctx, buf: UserPtr<slopos_abi::syscall::UserPerCpuStats>, max: u64) -> Result<u64, Errno>
{
    use core::sync::atomic::Ordering;
    use slopos_abi::syscall::UserPerCpuStats;

    let cpu_count = slopos_arch::pcr::get_cpu_count();
    let max_entries = (max as usize).min(cpu_count);

    for i in 0..max_entries {
        let stats = slopos_sched::per_cpu::with_cpu_scheduler(i, |sched| UserPerCpuStats {
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

        let dst_addr = buf
            .as_u64()
            .wrapping_add((i * core::mem::size_of::<UserPerCpuStats>()) as u64);
        let user_ptr = slopos_mm::user_ptr::UserPtr::<UserPerCpuStats>::try_new(dst_addr)
            .map_err(|_| Errno::EFAULT)?;
        copy_to_user(user_ptr, &stats).map_err(|_| Errno::EFAULT)?;
    }

    Ok(max_entries as u64)
});
