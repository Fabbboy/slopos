use core::ffi::c_char;
use core::ops::ControlFlow;
use core::sync::atomic::Ordering as AtomicOrdering;

use slopos_abi::Errno;
use slopos_abi::syscall::{CLOCK_MONOTONIC, CLOCK_REALTIME, Timespec, UserSysInfo};
use slopos_abi::task::{TaskExitReason, TaskFaultReason};
use slopos_abi::tty_error::TtyError;
use slopos_ostd::klog_debug;

use crate::syscall::args::{UserBytes, UserPtr};
use crate::syscall::common::{
    USER_IO_MAX_BYTES, syscall_bounded_from_user, syscall_copy_to_user_bounded,
};
use crate::syscall::result::SyscallResult;
use slopos_kernel_services::platform;
use slopos_kernel_services::syscall_services::tty;
use slopos_ostd::platform::power;
use slopos_sched::scheduler::{
    get_scheduler_stats, schedule, scheduler_is_preemption_enabled, sleep_current_task_ms, yield_,
};
use slopos_sched::task::{get_task_stats, task_terminate};

use slopos_mm::page_alloc::get_page_allocator_stats;
use slopos_mm::user_copy::copy_to_user;

define_syscall!(syscall_yield (ctx) cap(NoneSelf)
    -> SyscallResult {
    // rax is written before yielding and the handler returns `NoReturn`:
    // `yield_()` suspends, and the dispatcher's own `write_ok` on resume would
    // double-account the WL balance.
    ctx.write_ok(0);
    yield_();
    SyscallResult::NoReturn
});

define_syscall!(syscall_get_time_ms (ctx) cap(NoneSelf)
    -> u64 {
    slopos_kernel_services::clock::uptime_ms()
});

define_syscall!(syscall_clock_gettime
    (ctx, clock_id: u64, ts: UserPtr<Timespec>) cap(NoneSelf)
    -> Result<(), Errno>
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

define_syscall!(syscall_halt (ctx) cap(Power)
    -> SyscallResult {
    // The dispatcher already refused a caller lacking `Power`. The witness is
    // what the primitive itself demands, so a path reaching it without one
    // does not compile -- which is the whole point of moving the primitive
    // into ostd.
    let Ok(cap) = ctx.require_cap::<slopos_ostd::authority::Power>() else {
        return SyscallResult::Err(Errno::EPERM);
    };
    power::shutdown(&cap, b"user halt\0".as_ptr() as *const c_char);
    #[allow(unreachable_code)]
    SyscallResult::NoReturn
});

define_syscall!(syscall_reboot (ctx) cap(Power)
    -> SyscallResult {
    let Ok(cap) = ctx.require_cap::<slopos_ostd::authority::Power>() else {
        return SyscallResult::Err(Errno::EPERM);
    };
    power::reboot(&cap, b"user reboot\0".as_ptr() as *const c_char);
    #[allow(unreachable_code)]
    SyscallResult::NoReturn
});

define_syscall!(syscall_sleep_ms (ctx, ms: u64) cap(NoneSelf)
    -> Result<(), Errno> {
    let ms = ms.min(60000);
    if ms == 0 {
        return Ok(());
    }
    if scheduler_is_preemption_enabled() == 0 {
        slopos_kernel_services::platform::timer_poll_delay_ms(ms as u32);
        return Ok(());
    }

    // An absolute deadline, re-derived each pass: an early wake (a signal's
    // unblock, a kill's) must not end the sleep reporting success. EINTR rather
    // than ERESTARTSYS, because a restart re-arms the whole original duration.
    let deadline_ms = slopos_kernel_services::platform::get_time_ms().saturating_add(ms);
    loop {
        let now_ms = slopos_kernel_services::platform::get_time_ms();
        if now_ms >= deadline_ms {
            return Ok(());
        }
        let task = ctx.task();
        if task.is_killed() || slopos_sched::task::task_has_deliverable_signal(task) {
            return Err(Errno::EINTR);
        }
        let remaining = deadline_ms.saturating_sub(now_ms).min(u32::MAX as u64) as u32;
        if sleep_current_task_ms(remaining) != 0 {
            return Err(Errno::EINVAL);
        }
    }
});

define_syscall!(syscall_exit (ctx, code: u32) cap(NoneSelf)
    -> SyscallResult {
    let task_id = ctx.task_id();
    klog_debug!("SYSCALL_EXIT: task {} entering exit", task_id);
    {
        let t = ctx.task();
        // Atomic: written by any CPU terminating any task, never only by the owner.
        t.exit_reason
            .store(TaskExitReason::Normal.as_u16(), AtomicOrdering::Release);
        t.fault_reason
            .store(TaskFaultReason::None.as_u16(), AtomicOrdering::Release);
        t.exit_code.store(code, AtomicOrdering::Release);
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

// Not the `write(2)` a C program reaches through libc — that is
// `SYSCALL_FS_WRITE`, via the caller's fd table. This one exists for output
// that must survive a broken or absent fd 1. Deliberately unprivileged, like
// Linux's `/dev/kmsg`, and it reaches the same serialised writer klog uses, so
// a caller cannot interleave into a klog line or the harness's KTAP framing.
define_syscall!(syscall_user_write (ctx, buf: UserBytes) cap(ConsoleIo)
    -> Result<u64, Errno> {
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
    platform::console_write_serialized(&tmp[..write_len]);
    Ok(write_len as u64)
});

// `/dev/tty` semantics: the terminal resolves per process, so a task in a PTY
// session reads its own PTY and one with no controlling terminal reads nothing
// rather than the operator's console — `ENXIO`, as opening `/dev/tty` answers.
define_syscall!(syscall_user_read (ctx, buf: UserBytes) cap(NoneSelf)
    -> Result<u64, Errno> {
    if buf.base_u64() == 0 || buf.len() == 0 {
        return Err(Errno::EFAULT);
    }
    let tty_idx = ctx.task().controlling_tty().ok_or(Errno::ENXIO)?;
    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let max_len = buf.len().min(USER_IO_MAX_BYTES);
    let read_len = tty::read_cooked(tty_idx, tmp.as_mut_ptr(), max_len, false);
    let n = match read_len {
        Ok(n) => n,
        Err(TtyError::Restart) => return Err(Errno::ERESTARTSYS),
        Err(_) => return Err(Errno::EINVAL),
    };
    syscall_copy_to_user_bounded(buf.base_u64(), &tmp[..n]).map_err(|_| Errno::EFAULT)?;
    Ok(n as u64)
});

define_syscall!(syscall_sys_info (ctx, info_out: UserPtr<UserSysInfo>) cap(SysInspect)
    -> Result<(), Errno> {
    let pages = get_page_allocator_stats();
    let tasks = get_task_stats();
    let sched = get_scheduler_stats();

    let info = UserSysInfo {
        total_pages: pages.total,
        free_pages: pages.free,
        allocated_pages: pages.allocated,
        total_tasks: tasks.total_tasks,
        active_tasks: tasks.active_tasks,
        task_context_switches: tasks.context_switches,
        scheduler_context_switches: sched.context_switches,
        scheduler_yields: sched.yields,
        ready_tasks: sched.ready_tasks,
        schedule_calls: sched.schedule_calls,
        wl_balance: slopos_ostd::wl_currency::check_balance(),
        boot_flags: slopos_ostd::boot_flags::get_flags(),
    };

    copy_to_user(info_out.inner(), &info).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_process_list
    (ctx, buf: UserPtr<slopos_abi::syscall::UserTaskEntry>, max: u64) cap(SysInspect)
    -> Result<u64, Errno>
{
    use slopos_abi::syscall::UserTaskEntry;
    use slopos_abi::task::{INVALID_TASK_ID, MAX_TASKS};
    use slopos_ostd::KVec;
    use slopos_sched::task::task_try_for_each_enumerable;
    use crate::syscall::signal::{signal_dominates, signal_is_init, signal_may_name};

    // Enumeration answers to the same relation `kill` does, so an id this
    // refuses to report is also one `kill` would refuse to act on. `PROC_ADMIN`
    // (held by `/bin/sysmon`) sees everything.
    let caller_flags = ctx.task().flags;
    let unrestricted = ctx.is_proc_admin();
    let visible = |task: &slopos_sched::task_struct::Task| {
        unrestricted
            || (signal_may_name(task.flags)
                && !signal_is_init(task.task_id)
                && signal_dominates(caller_flags, task.flags))
    };

    let max_entries = (max as usize).min(MAX_TASKS);
    let mut entries = match KVec::<UserTaskEntry>::with_capacity(max_entries) {
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

    let mut count = 0usize;
    task_try_for_each_enumerable(|task| {
        if count >= max_entries {
            return ControlFlow::Break(());
        }
        if task.task_id == INVALID_TASK_ID {
            return ControlFlow::Continue(());
        }
        if !visible(task) {
            return ControlFlow::Continue(());
        }
        let entry = &mut entries[count];
        entry.task_id = task.task_id;
        entry.parent_task_id = task.parent_task_id();
        entry.process_id = task.process_id;
        entry.state = task.status().as_u8();
        entry.block_reason = task.load_block_reason().as_u8();
        entry.priority = task.priority.as_u8();
        entry.last_cpu = task.last_cpu();
        entry.cpu_affinity = task.cpu_affinity();
        entry.total_runtime_us =
            slopos_kernel_services::clock::ticks_to_microseconds(task.total_runtime());
        entry.creation_time_ms = task.creation_time;
        entry.yield_count = task.yield_count();
        entry.name = task.name;
        count += 1;
        ControlFlow::Continue(())
    });

    for i in 0..count {
        let dst_addr = buf
            .as_u64()
            .wrapping_add((i * core::mem::size_of::<UserTaskEntry>()) as u64);
        let user_ptr = slopos_mm::user_ptr::UserPtr::<UserTaskEntry>::try_new(dst_addr)
            .map_err(|_| Errno::EFAULT)?;
        copy_to_user(user_ptr, &entries[i]).map_err(|_| Errno::EFAULT)?;
    }

    Ok(count as u64)
});

define_syscall!(syscall_cpu_info
    (ctx, info_out: UserPtr<slopos_abi::syscall::UserCpuInfo>) cap(SysInspect)
    -> Result<(), Errno>
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
    (ctx, buf: UserPtr<slopos_abi::syscall::UserPerCpuStats>, max: u64) cap(SysInspect)
    -> Result<u64, Errno>
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
