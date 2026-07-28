use core::sync::atomic::Ordering;
use slopos_abi::Errno;
use slopos_abi::fs::FS_TYPE_DIRECTORY;
use slopos_abi::spawn::{SPAWN_MAX_FD_ACTIONS, SpawnAttrs, SpawnFdAction, SpawnFdActionKind};
use slopos_abi::syscall::{ARCH_GET_FS, ARCH_SET_FS, ENOSYS_RETURN, FUTEX_WAIT, FUTEX_WAKE};
use slopos_abi::task::TaskPriority;
use slopos_fs::vfs::traits::VfsError;
use slopos_ostd::KVec;
use slopos_ostd::task::{new_group_in_session, new_session_group};
use slopos_ostd::user::context::UserContext;
use slopos_sched::scheduler::{task_apply_affinity, task_wait_for};
use slopos_sched::task::{
    task_borrow, task_borrow_mut, task_consume_zombie, task_cpu_affinity,
    task_default_signals_in_mask, task_find_by_id, task_fork, task_peek_exit_info, task_pgid,
    task_process_group, task_reset_caught_handlers, task_session, task_set_cpu_affinity,
    task_set_fs_base, task_terminate,
};
use slopos_sched::task_struct::Current;

use slopos_arch::cpu;
use slopos_mm::user_copy::{copy_from_user, copy_to_user};
use slopos_mm::user_ptr::UserPtr as MmUserPtr;

use crate::exec;
use crate::syscall::args::{Tid, UserBytes, UserCStr, UserPtr};
use crate::syscall::common::{
    USER_PATH_MAX, syscall_bounded_from_user, syscall_copy_to_user_bounded, syscall_copy_user_str,
};
use crate::syscall::result::SyscallResult;

fn read_user_ptr_array_terminated(base_ptr: u64, max_count: usize) -> Result<KVec<u64>, ()> {
    let mut out = KVec::<u64>::with_capacity(max_count).map_err(|_| ())?;

    for idx in 0..max_count {
        let slot_addr = base_ptr
            .checked_add((idx * core::mem::size_of::<u64>()) as u64)
            .ok_or(())?;
        let user_slot = MmUserPtr::<u64>::try_new(slot_addr).map_err(|_| ())?;
        let value = copy_from_user(user_slot).map_err(|_| ())?;
        if value == 0 {
            return Ok(out);
        }
        out.push(value).map_err(|_| ())?;
    }

    Err(())
}

fn read_user_ptr_array_count(
    base_ptr: u64,
    count: usize,
    max_count: usize,
) -> Result<KVec<u64>, ()> {
    if count > max_count {
        return Err(());
    }

    let mut out = KVec::<u64>::with_capacity(count).map_err(|_| ())?;

    for idx in 0..count {
        let slot_addr = base_ptr
            .checked_add((idx * core::mem::size_of::<u64>()) as u64)
            .ok_or(())?;
        let user_slot = MmUserPtr::<u64>::try_new(slot_addr).map_err(|_| ())?;
        let value = copy_from_user(user_slot).map_err(|_| ())?;
        if value == 0 {
            break;
        }
        out.push(value).map_err(|_| ())?;
    }

    Ok(out)
}

fn read_user_cstr_list(ptrs: &[u64]) -> Result<KVec<KVec<u8>>, ()> {
    let mut out = KVec::<KVec<u8>>::with_capacity(ptrs.len()).map_err(|_| ())?;

    let mut buf = KVec::<u8>::zeroed(exec::EXEC_MAX_ARG_STRLEN).map_err(|_| ())?;

    for &ptr in ptrs {
        for b in buf.as_mut_slice().iter_mut() {
            *b = 0;
        }
        syscall_copy_user_str(buf.as_mut_slice(), ptr).map_err(|_| ())?;
        let len = buf
            .as_slice()
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(buf.len());

        let mut s = KVec::<u8>::with_capacity(len).map_err(|_| ())?;
        s.extend_from_slice(&buf.as_slice()[..len])
            .map_err(|_| ())?;
        out.push(s).map_err(|_| ())?;
    }

    Ok(out)
}

/// Copy an `Open` action's path out of user memory into a kernel buffer,
/// trimming at the first NUL (accepts both explicit-length and NUL-terminated
/// paths).
fn read_open_action_path(ptr: u64, len: u64) -> Result<KVec<u8>, Errno> {
    if ptr == 0 || len == 0 {
        return Err(Errno::EINVAL);
    }
    let mut tmp = [0u8; exec::EXEC_MAX_PATH];
    let copied = syscall_bounded_from_user(&mut tmp, ptr, len, exec::EXEC_MAX_PATH)
        .map_err(|_| Errno::EFAULT)?;
    let bytes = &tmp[..copied];
    let bytes = match bytes.iter().position(|&b| b == 0) {
        Some(nul) => &bytes[..nul],
        None => bytes,
    };
    if bytes.is_empty() {
        return Err(Errno::EINVAL);
    }
    let mut buf = KVec::<u8>::with_capacity(bytes.len()).map_err(|_| Errno::ENOMEM)?;
    buf.extend_from_slice(bytes).map_err(|_| Errno::ENOMEM)?;
    Ok(buf)
}

/// Decode the spawn fd-action array from user memory into kernel-owned
/// [`exec::FdAction`]s (`Open` paths copied in). Bounded by
/// [`SPAWN_MAX_FD_ACTIONS`].
fn read_user_spawn_actions(attrs: &SpawnAttrs) -> Result<KVec<exec::FdAction>, Errno> {
    let count = attrs.actions_len as usize;
    if count == 0 {
        return Ok(KVec::new());
    }
    if count > SPAWN_MAX_FD_ACTIONS {
        return Err(Errno::EINVAL);
    }
    if attrs.actions_ptr == 0 {
        return Err(Errno::EFAULT);
    }

    let mut out = KVec::<exec::FdAction>::with_capacity(count).map_err(|_| Errno::ENOMEM)?;
    for idx in 0..count {
        let slot_addr = attrs
            .actions_ptr
            .checked_add((idx * core::mem::size_of::<SpawnFdAction>()) as u64)
            .ok_or(Errno::EFAULT)?;
        let user_slot =
            MmUserPtr::<SpawnFdAction>::try_new(slot_addr).map_err(|_| Errno::EFAULT)?;
        let raw = copy_from_user(user_slot).map_err(|_| Errno::EFAULT)?;
        let action = match SpawnFdActionKind::from_u32(raw.kind).ok_or(Errno::EINVAL)? {
            SpawnFdActionKind::CloneFd => exec::FdAction::Clone {
                src_fd: raw.src_fd,
                target_fd: raw.target_fd,
            },
            SpawnFdActionKind::TransferFd => exec::FdAction::Transfer {
                src_fd: raw.src_fd,
                target_fd: raw.target_fd,
            },
            SpawnFdActionKind::Close => exec::FdAction::Close {
                target_fd: raw.target_fd,
            },
            SpawnFdActionKind::Open => exec::FdAction::Open {
                target_fd: raw.target_fd,
                path: read_open_action_path(raw.open_path_ptr, raw.open_path_len)?,
                flags: raw.open_flags,
            },
        };
        out.push(action).map_err(|_| Errno::ENOMEM)?;
    }
    Ok(out)
}

define_syscall!(syscall_spawn_path
    (ctx, path: UserBytes, argv_ptr: u64, argc_raw: u32, attrs_ptr: u64)
    -> Result<u64, Errno>
{
    if path.base_u64() == 0 || path.len() == 0 || path.len() > exec::EXEC_MAX_PATH {
        return Err(Errno::EINVAL);
    }
    if attrs_ptr == 0 {
        return Err(Errno::EFAULT);
    }

    let attrs_user = MmUserPtr::<SpawnAttrs>::try_new(attrs_ptr).map_err(|_| Errno::EFAULT)?;
    let attrs = copy_from_user(attrs_user).map_err(|_| Errno::EFAULT)?;

    let priority = TaskPriority::try_from_u8(attrs.priority).ok_or(Errno::EINVAL)?;
    // KernelIo is reserved for kernel kthreads (NAPI, net-timer, …).
    // User space must not be able to spawn at that tier or it would
    // starve every other user task. The kernel-side spawn surface is
    // `slopos_ostd::task::spawn_kernel_io`, which takes a typed
    // `KernelIoToken` witness — there is no syscall analogue.
    if matches!(priority, TaskPriority::KernelIo) {
        return Err(Errno::EINVAL);
    }
    let argc = argc_raw as usize;

    let mut path_buf = [0u8; exec::EXEC_MAX_PATH];
    let copied_len = syscall_bounded_from_user(
        &mut path_buf,
        path.base_u64(),
        path.len() as u64,
        exec::EXEC_MAX_PATH,
    )
    .map_err(|_| Errno::EFAULT)?;

    let argv_storage = if argv_ptr != 0 && argc > 0 {
        let argv_ptrs = read_user_ptr_array_count(argv_ptr, argc, exec::EXEC_MAX_ARGS)
            .map_err(|_| Errno::EINVAL)?;
        Some(read_user_cstr_list(argv_ptrs.as_slice()).map_err(|_| Errno::EFAULT)?)
    } else {
        None
    };

    let argv_refs = match argv_storage
        .as_ref()
        .map(|values| KVec::<&[u8]>::from_iter_fallible(values.iter().map(|v| v.as_slice())))
    {
        Some(Ok(refs)) => Some(refs),
        Some(Err(_)) => return Err(Errno::ENOMEM),
        None => None,
    };

    let actions = read_user_spawn_actions(&attrs)?;

    let parent_pid = ctx.process_id();
    let parent_tid = ctx.task_id();
    match exec::spawn_program_with_attrs(
        &path_buf[..copied_len],
        argv_refs.as_deref(),
        priority,
        attrs.flags,
        actions.as_slice(),
        attrs.sigdefault_mask,
        parent_pid,
        parent_tid,
    ) {
        Ok(task_id) => Ok(task_id as u64),
        Err(err) => Ok((err as i32) as u64),
    }
});

define_syscall!(syscall_sigdefault
    (ctx, mask: u64) -> Result<u64, Errno>
{
    if let Some(task) = Some(ctx.task()) {
        task_default_signals_in_mask(task, mask);
    }
    Ok(0)
});

define_syscall!(syscall_waitpid
    (ctx, target: Tid, flags: u32) -> Result<u64, Errno>
{
    let target_id = target.raw();
    let wnohang = (flags & 0x1) != 0;
    if target_id == 0 {
        return Err(Errno::EINVAL);
    }

    if let Some(info) = task_consume_zombie(target_id) {
        return Ok(info.exit_code as u64);
    }

    if wnohang {
        return if task_find_by_id(target_id).is_none() {
            Err(Errno::ECHILD)
        } else {
            Err(Errno::EAGAIN)
        };
    }

    task_wait_for(target_id);

    if let Some(info) = task_consume_zombie(target_id) {
        Ok(info.exit_code as u64)
    } else if let Some(info) = task_peek_exit_info(target_id) {
        Ok(info.exit_code as u64)
    } else {
        Err(Errno::ECHILD)
    }
});

define_syscall!(syscall_terminate_task
    (ctx, target: Tid)
    requires(compositor)
    -> Result<(), Errno>
{
    let target_id = target.raw();
    if target_id == 0 {
        return Err(Errno::EINVAL);
    }
    let caller_id = ctx.task_id();
    if target_id == caller_id {
        return Err(Errno::EINVAL);
    }
    if task_terminate(target_id) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
});

define_syscall!(syscall_exec
    (ctx, path_ptr: u64, argv_ptr: u64, envp_ptr: u64)
    requires(let process_id: process_id)
    -> SyscallResult
{
    if path_ptr == 0 {
        return SyscallResult::Err(Errno::EFAULT);
    }

    let mut path_buf = [0u8; exec::EXEC_MAX_PATH];
    if syscall_copy_user_str(&mut path_buf, path_ptr).is_err() {
        return SyscallResult::Err(Errno::EFAULT);
    }

    let path_len = path_buf
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(path_buf.len());
    let path = &path_buf[..path_len];

    let argv_storage = if argv_ptr != 0 {
        match read_user_ptr_array_terminated(argv_ptr, exec::EXEC_MAX_ARGS) {
            Ok(argv_ptrs) => match read_user_cstr_list(argv_ptrs.as_slice()) {
                Ok(values) => Some(values),
                Err(_) => return SyscallResult::Err(Errno::EFAULT),
            },
            Err(_) => return SyscallResult::Err(Errno::EINVAL),
        }
    } else {
        None
    };

    let envp_storage = if envp_ptr != 0 {
        match read_user_ptr_array_terminated(envp_ptr, exec::EXEC_MAX_ENVS) {
            Ok(envp_ptrs) => match read_user_cstr_list(envp_ptrs.as_slice()) {
                Ok(values) => Some(values),
                Err(_) => return SyscallResult::Err(Errno::EFAULT),
            },
            Err(_) => return SyscallResult::Err(Errno::EINVAL),
        }
    } else {
        None
    };

    let argv_refs = match argv_storage
        .as_ref()
        .map(|values| KVec::<&[u8]>::from_iter_fallible(values.iter().map(|v| v.as_slice())))
    {
        Some(Ok(refs)) => Some(refs),
        Some(Err(_)) => return SyscallResult::Err(Errno::ENOMEM),
        None => None,
    };
    let envp_refs = match envp_storage
        .as_ref()
        .map(|values| KVec::<&[u8]>::from_iter_fallible(values.iter().map(|v| v.as_slice())))
    {
        Some(Ok(refs)) => Some(refs),
        Some(Err(_)) => return SyscallResult::Err(Errno::ENOMEM),
        None => None,
    };

    let mut entry_point = 0u64;
    let mut stack_ptr = 0u64;
    let mut tls_tp = 0u64;

    let irq_was_enabled = cpu::are_interrupts_enabled();
    if !irq_was_enabled {
        cpu::enable_interrupts();
    }

    let exec_result = exec::do_exec(
        process_id,
        path,
        argv_refs.as_deref(),
        envp_refs.as_deref(),
        &mut entry_point,
        &mut stack_ptr,
        &mut tls_tp,
    );

    if !irq_was_enabled {
        cpu::disable_interrupts();
    }

    match exec_result {
        Ok(()) => {
            if tls_tp != 0 {
                let user_tp = match MmUserPtr::<u64>::try_new(tls_tp) {
                    Ok(ptr) => ptr,
                    Err(_) => return SyscallResult::Err(Errno::EFAULT),
                };
                if copy_to_user(user_tp, &tls_tp).is_err() {
                    return SyscallResult::Err(Errno::EFAULT);
                }
            }

            // Point of no return: old image is gone. Tear down task-bound
            // resources (compositor surface, shm buffers, input queues, ...).
            let task_id = ctx.task_id();
            slopos_sched::task::task_cleanup_for_exec(task_id);

            // Reset caught signal handlers to SIG_DFL so a stale handler
            // pointer never survives into the new image; SIG_IGN and the
            // blocked/pending state are preserved (POSIX exec semantics).
            if let Some(task) = Some(ctx.task()) {
                task_reset_caught_handlers(task);
            }

            if tls_tp != 0 {
                {
        let t = ctx.task();
                    task_set_fs_base(t, tls_tp);
                }
                slopos_arch::cpu::msr::write_msr(slopos_arch::cpu::msr::Msr::FS_BASE, tls_tp);
            }
            // Build the new user-mode entry register snapshot, then commit
            // through `set_regs` so CS/SS/RFLAGS sandbox bits are reapplied.
            let uc = ctx.user_ctx_mut();
            let mut regs = *uc.regs();
            regs.rip = entry_point;
            regs.rsp = stack_ptr;
            regs.rax = 0;
            regs.rdi = 0;
            regs.rsi = 0;
            regs.rdx = 0;
            regs.rcx = 0;
            regs.r8 = 0;
            regs.r9 = 0;
            regs.r10 = 0;
            regs.r11 = 0;
            uc.set_regs(regs);

            // The new image must start with a clean FPU/vector file — never
            // leak the previous program's XMM/YMM contents. Reset the stored
            // state AND load the default into the CPU under IRQ-off, so a
            // context switch can't re-save the old image's live registers
            // over the reset before the new image runs.
            let xcr0 = slopos_ostd::cpu::x86_64::xsave::active_xcr0();
            slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
                // The exec'ing task is this CPU's current, so the guard is the
                // witness that authorises both writes. One witness covers the
                // reset and the load, so they cannot be split by a second
                // derivation, and both maintain the FPU owner tag.
                if let Some(current) = slopos_sched::task_struct::Current::get() {
                    current.task().fpu_reset(&current);
                    current.task().fpu_restore_to_cpu(&current, xcr0);
                }
            });
            SyscallResult::NoReturn
        }
        Err(e) => SyscallResult::Err(Errno::from_raw(e as i32).unwrap_or(Errno::EINVAL)),
    }
});

define_syscall!(syscall_get_cpu_count (ctx) -> Result<u64, Errno> {
    Ok(slopos_arch::pcr::get_cpu_count() as u64)
});

define_syscall!(syscall_get_current_cpu (ctx) -> Result<u64, Errno> {
    Ok(slopos_arch::pcr::get_current_cpu() as u64)
});

define_syscall!(syscall_set_cpu_affinity
    (ctx, target: u32, new_affinity: u32)
    requires(let task_id: task_id)
    -> Result<(), Errno>
{
    let resolved = if target == 0 { task_id } else { target };
    let Some(task_ref) = task_find_by_id(resolved) else {
        return Err(Errno::ESRCH);
    };
    let task_ptr = task_ref.as_ptr();
    task_set_cpu_affinity(task_ptr, new_affinity);
    // Stamping the mask is not enough — re-place the task so the new mask
    // actually governs where it runs (Linux `sched_setaffinity` → migrate).
    task_apply_affinity(task_ref.arc(), new_affinity);
    Ok(())
});

define_syscall!(syscall_get_cpu_affinity
    (ctx, target: u32)
    requires(let task_id: task_id)
    -> Result<u64, Errno>
{
    let resolved = if target == 0 { task_id } else { target };
    let Some(task_ref) = task_find_by_id(resolved) else {
        return Err(Errno::ESRCH);
    };
    let task_ptr = task_ref.as_ptr();
    Ok(task_cpu_affinity(task_ptr).unwrap_or(0) as u64)
});

define_syscall!(syscall_getpid (ctx)
    requires(let task_id: task_id)
    -> Result<u32, Errno>
{
    Ok(task_id)
});

define_syscall!(syscall_getppid (ctx) -> Result<u32, Errno> {
    let task = ctx.task();
    Ok(task.parent_task_id)
});

define_syscall!(syscall_getpgid
    (ctx, target: u32)
    requires(let task_id: task_id)
    -> Result<u32, Errno>
{
    let resolved = if target == 0 { task_id } else { target };
    let Some(task_ref) = task_find_by_id(resolved) else {
        return Err(Errno::ESRCH);
    };
    let task_ptr = task_ref.as_ptr();
    Ok(task_pgid(task_ptr).unwrap_or(0))
});

define_syscall!(syscall_setpgid
    (ctx, pid: u32, pgid_arg: u32)
    requires(let task_id: task_id)
    -> Result<(), Errno>
{
    let resolved_pid = if pid == 0 { task_id } else { pid };
    let resolved_pgid = if pgid_arg == 0 { resolved_pid } else { pgid_arg };

    let Some(caller_ref) = task_find_by_id(task_id) else {
        return Err(Errno::EINVAL);
    };
    let Some(target_ref) = task_find_by_id(resolved_pid) else {
        return Err(Errno::EINVAL);
    };
    if resolved_pgid == 0 {
        return Err(Errno::EINVAL);
    }
    let caller_ptr = caller_ref.as_ptr();
    let target_ptr = target_ref.as_ptr();

    let caller = task_borrow(caller_ptr).ok_or(Errno::EINVAL)?;
    let caller_sid = caller.sid;
    let target = task_borrow_mut(target_ptr).ok_or(Errno::EINVAL)?;
    if resolved_pid != task_id && target.parent_task_id != task_id {
        return Err(Errno::EINVAL);
    }
    if target.sid != caller_sid {
        return Err(Errno::EINVAL);
    }

    // Resolve the group object the target should carry, mirroring the integer
    // pgid it is about to hold.
    let new_group = if resolved_pgid == resolved_pid {
        // Become a new group leader within the current session — unless the
        // target already leads exactly this group.
        match task_process_group(target_ptr) {
            Some(existing) if existing.id() == resolved_pgid => Some(existing),
            _ => {
                let session = task_session(target_ptr).ok_or(Errno::EPERM)?;
                Some(new_group_in_session(resolved_pgid, session).ok_or(Errno::ENOMEM)?)
            }
        }
    } else {
        // Join an existing group: its leader must exist and share the session.
        let Some(leader_ref) = task_find_by_id(resolved_pgid) else {
            return Err(Errno::EINVAL);
        };
        let leader_ptr = leader_ref.as_ptr();
        let leader = task_borrow(leader_ptr).ok_or(Errno::EINVAL)?;
        if leader.sid != caller_sid {
            return Err(Errno::EINVAL);
        }
        Some(task_process_group(leader_ptr).ok_or(Errno::EINVAL)?)
    };

    target.pgid = resolved_pgid;
    // `target` is generally *not* the calling task, so this write lands on a
    // field a reader on another CPU may be cloning from right now. The
    // displaced membership is released after a grace period, which is what
    // keeps a concurrent reader's clone from racing its destructor.
    target.process_group.store(new_group);
    Ok(())
});

define_syscall!(syscall_setsid (ctx)
    requires(let task_id: task_id)
    -> Result<u32, Errno>
{
    let Some(task_ref) = task_find_by_id(task_id) else {
        return Err(Errno::EINVAL);
    };
    let task_ptr = task_ref.as_ptr();
    let task = task_borrow_mut(task_ptr).ok_or(Errno::EINVAL)?;
    if task.pgid == task.task_id || task.sid == task.task_id {
        return Err(Errno::EPERM);
    }
    // A fresh session + its initial group; installing them drops the old group
    // membership (so the old session and any terminal weak links to it die).
    let pg = new_session_group(task.task_id).ok_or(Errno::ENOMEM)?;
    if task.controlling_tty().is_some() {
        task.set_controlling_tty(None);
    }
    task.sid = task.task_id;
    task.pgid = task.task_id;
    task.process_group.store(Some(pg));
    Ok(task.sid)
});

define_syscall!(syscall_getuid (ctx) -> u32 { 0 });
define_syscall!(syscall_getgid (ctx) -> u32 { 0 });
define_syscall!(syscall_geteuid (ctx) -> u32 { 0 });
define_syscall!(syscall_getegid (ctx) -> u32 { 0 });

define_syscall!(syscall_chdir
    (ctx, path: UserCStr<USER_PATH_MAX>) -> Result<(), Errno>
{
    if path.is_empty() {
        return Err(Errno::EINVAL);
    }
    match slopos_fs::vfs::ops::vfs_stat(path.as_bytes()) {
        Ok((file_type, _size)) => {
            if file_type != FS_TYPE_DIRECTORY {
                return Err(Errno::ENOTDIR);
            }
        }
        Err(VfsError::NotDirectory) => return Err(Errno::ENOTDIR),
        Err(VfsError::InvalidPath) => return Err(Errno::EINVAL),
        Err(_) => return Err(Errno::ENOENT),
    }

    // The working directory is written under the exclusivity witness rather
    // than a `&mut Task`: only the running task touches its own cwd, and that
    // is precisely what `CurrentTask` proves.
    let current = Current::get().ok_or(Errno::EINVAL)?;
    if !current.task().set_cwd(&current, path.as_bytes()) {
        return Err(Errno::ENAMETOOLONG);
    }
    Ok(())
});

define_syscall!(syscall_getcwd
    (ctx, buf: UserBytes) -> Result<u64, Errno>
{
    if buf.base_u64() == 0 {
        return Err(Errno::EFAULT);
    }
    let current = Current::get().ok_or(Errno::EINVAL)?;
    current.task().with_cwd(&current, |cwd| {
        if buf.len() < cwd.len() {
            return Err(Errno::ERANGE);
        }
        syscall_copy_to_user_bounded(buf.base_u64(), cwd).map_err(|_| Errno::EFAULT)?;
        Ok(cwd.len() as u64)
    })
});

define_syscall!(syscall_arch_prctl
    (ctx, cmd: u64, addr: u64) -> Result<(), Errno>
{
    match cmd {
        ARCH_SET_FS => {
            if addr >= slopos_mm::memory_layout_defs::USER_SPACE_END_VA && addr != 0 {
                return Err(Errno::EINVAL);
            }
            let t = ctx.task();
            t.fs_base.store(addr, Ordering::Release);
            slopos_arch::cpu::msr::write_msr(slopos_arch::cpu::msr::Msr::FS_BASE, addr);
            Ok(())
        }
        ARCH_GET_FS => {
            if addr == 0 {
                return Err(Errno::EINVAL);
            }
            let t = ctx.task();
            let fs_base_val = t.fs_base.load(Ordering::Acquire);
            let user_ptr = MmUserPtr::<u64>::try_new(addr).map_err(|_| Errno::EFAULT)?;
            copy_to_user(user_ptr, &fs_base_val).map_err(|_| Errno::EFAULT)?;
            Ok(())
        }
        _ => Err(Errno::EINVAL),
    }
});

define_syscall!(syscall_fork (ctx) -> Result<u64, Errno> {
    let task = ctx.task();
    let user_ctx_ptr = ctx.user_ctx_ptr() as *const UserContext;
    let child_id = task_fork(task, user_ctx_ptr);
    if child_id == slopos_abi::task::INVALID_TASK_ID {
        Err(Errno::EAGAIN)
    } else {
        Ok(child_id as u64)
    }
});

define_syscall!(syscall_clone
    (ctx, flags: u64, child_stack: u64, parent_tidptr: u64, child_tidptr: u64, tls: u64)
    -> Result<u64, Errno>
{
    let parent = ctx.task();
    match slopos_sched::task::task_clone(
        parent,
        ctx.user_ctx_ptr() as *const UserContext,
        flags,
        child_stack,
        parent_tidptr,
        child_tidptr,
        tls,
    ) {
        Ok(child_id) => Ok(child_id as u64),
        Err(errno) => Err(Errno::from_raw(errno as i32).unwrap_or(Errno::EINVAL)),
    }
});

define_syscall!(syscall_futex
    (ctx, uaddr: u64, op: u64, val: u32, timeout: u64) -> Result<u64, Errno>
{
    if (uaddr & 0x3) != 0 {
        return Err(Errno::EINVAL);
    }

    let user_word = MmUserPtr::<u32>::try_new(uaddr).map_err(|_| Errno::EFAULT)?;
    if copy_from_user(user_word).is_err() {
        return Err(Errno::EFAULT);
    }

    let rc = match op {
        FUTEX_WAIT => slopos_sched::futex::futex_wait(uaddr, val, timeout),
        FUTEX_WAKE => slopos_sched::futex::futex_wake(uaddr, val),
        _ => ENOSYS_RETURN as i64,
    };

    Ok(rc as u64)
});

define_syscall!(syscall_vhangup (ctx)
    requires(let task_id: task_id)
    -> Result<(), Errno>
{
    let Some(task_ref) = task_find_by_id(task_id) else {
        return Err(Errno::EINVAL);
    };
    let task_ptr = task_ref.as_ptr();
    let task = task_borrow(task_ptr).ok_or(Errno::EINVAL)?;
    let ctty = match task.controlling_tty() {
        Some(idx) => idx,
        None => return Err(Errno::EPERM),
    };
    slopos_kernel_services::syscall_services::tty::hangup(ctty);
    Ok(())
});

#[allow(dead_code)]
type _Unused<T> = UserPtr<T>;
