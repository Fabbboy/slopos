use core::sync::atomic::Ordering;
use slopos_abi::Errno;
use slopos_abi::fs::FS_TYPE_DIRECTORY;
use slopos_abi::spawn::{SPAWN_MAX_FD_ACTIONS, SpawnAttrs, SpawnFdAction, SpawnFdActionKind};
use slopos_abi::syscall::{ARCH_GET_FS, ARCH_SET_FS, ENOSYS_RETURN, FUTEX_WAIT, FUTEX_WAKE};
use slopos_abi::task::{
    SPAWN_PRIVILEGED, SPAWN_RESERVED, SPAWN_USER_SETTABLE, TASK_FLAG_KERNEL_MODE, TaskPriority,
};
use slopos_fs::fileio::FdTable;
use slopos_fs::vfs::traits::VfsError;
use slopos_ostd::KVec;
use slopos_ostd::task::{new_group_in_session, new_session_group};
use slopos_sched::scheduler::{task_apply_affinity, task_wait_for};
use slopos_sched::task::{
    task_consume_zombie, task_default_signals_in_mask, task_find_by_id, task_fork,
    task_peek_exit_info, task_reset_caught_handlers, task_terminate,
};
use slopos_sched::task_struct::Current;

use slopos_arch::cpu;
use slopos_mm::user_copy::{copy_from_user, copy_to_user};
use slopos_mm::user_ptr::UserPtr as MmUserPtr;

use crate::exec;
use crate::syscall::args::{Tid, UserBytes, UserCStr, UserPtr, WaitTarget};
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

/// Copy an `Open` action's path into a kernel buffer, trimming at the first NUL
/// so both explicit-length and NUL-terminated paths are accepted.
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

/// Classify the caller-supplied `SpawnAttrs::flags`, returning the subset the
/// child is allowed to inherit from the request.
///
/// Order is load-bearing: an undefined bit is answered as malformed before
/// anything is said about privilege, so probing reserved bits never learns from
/// an `EPERM` that a bit *means* something. The privileged bits a child ends up
/// with come from [`crate::exec::grants`].
fn validate_spawn_flags(flags: u16) -> Result<u16, Errno> {
    if flags & SPAWN_RESERVED != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & TASK_FLAG_KERNEL_MODE != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & SPAWN_PRIVILEGED != 0 {
        return Err(Errno::EPERM);
    }
    // `USER_MODE` is dropped rather than refused: `spawn_program_with_attrs`
    // ORs it back in unconditionally.
    Ok(flags & SPAWN_USER_SETTABLE)
}

define_syscall!(syscall_spawn_path
    (ctx, path: UserBytes, argv_ptr: u64, argc_raw: u32, attrs_ptr: u64)
    cap(NoneSelf)
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
    // Userland picks between the two ordinary tiers and nothing else. `High` is
    // handed out by program identity (`exec::grants`) — a `loop {}` binary at
    // that tier starves the machine.
    if !matches!(priority, TaskPriority::Normal | TaskPriority::Low) {
        return Err(Errno::EINVAL);
    }
    let flags = validate_spawn_flags(attrs.flags)?;
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

    let envp_storage = if attrs.envp_ptr != 0 && attrs.envp_len > 0 {
        let envp_ptrs = read_user_ptr_array_count(
            attrs.envp_ptr,
            attrs.envp_len as usize,
            exec::EXEC_MAX_ENVS,
        )
        .map_err(|_| Errno::EINVAL)?;
        Some(read_user_cstr_list(envp_ptrs.as_slice()).map_err(|_| Errno::EFAULT)?)
    } else {
        None
    };

    let envp_refs = match envp_storage
        .as_ref()
        .map(|values| KVec::<&[u8]>::from_iter_fallible(values.iter().map(|v| v.as_slice())))
    {
        Some(Ok(refs)) => Some(refs),
        Some(Err(_)) => return Err(Errno::ENOMEM),
        None => None,
    };

    let actions = read_user_spawn_actions(&attrs)?;

    // The spawner's own table, so the child's fd actions clone from the process
    // that asked rather than from whoever holds its number by then.
    let parent_table = ctx.require_process().ok();
    let parent_tid = ctx.task_id();
    match exec::spawn_program_with_attrs(
        &path_buf[..copied_len],
        argv_refs.as_deref(),
        envp_refs.as_deref(),
        priority,
        flags,
        actions.as_slice(),
        attrs.sigdefault_mask,
        parent_table,
        parent_tid,
    ) {
        Ok(task_id) => Ok(task_id as u64),
        Err(err) => Ok((err as i32) as u64),
    }
});

define_syscall!(syscall_sigdefault
    (ctx, mask: u64) cap(NoneSelf)
    -> Result<u64, Errno>
{
    if let Some(task) = Some(ctx.task()) {
        task_default_signals_in_mask(task, mask);
    }
    Ok(0)
});

define_syscall!(syscall_waitpid
    (ctx, target: WaitTarget, flags: u32) cap(NoneRelation)
    -> Result<u64, Errno>
{
    let wnohang = (flags & 0x1) != 0;
    let caller_id = ctx.task_id();

    // Wait-any resolves to a concrete child first, so the ownership check and
    // the reap below stay one implementation.
    let target_id = match target {
        WaitTarget::Child(id) => id,
        WaitTarget::Any => match slopos_sched::task::task_first_exited_child(caller_id) {
            Some(id) => id,
            None => {
                if !slopos_sched::task::task_has_children(caller_id) {
                    return Err(Errno::ECHILD);
                }
                if wnohang {
                    return Err(Errno::EAGAIN);
                }
                slopos_sched::task::task_wait_any_child(caller_id)?;
                match slopos_sched::task::task_first_exited_child(caller_id) {
                    Some(id) => id,
                    None => return Err(Errno::ECHILD),
                }
            }
        },
    };

    // Reaping is the parent's alone: `task_consume_zombie` drops the parent's
    // owning reference, so a stranger's wait would leave the real parent with
    // `ECHILD`.
    match task_find_by_id(target_id) {
        Some(t) if t.parent_task_id() == caller_id => {}
        _ => return Err(Errno::ECHILD),
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
    cap(NoneRelation)
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
    // Resolved and authorized in one step, and the authorization *carries the
    // target*. This handler used to carry `requires(compositor)` and then
    // terminate `target_id` with only a self-exclusion, which is the shape a
    // bare witness cannot fix: the variable that is checked must be the
    // variable subsequently used.
    let target = crate::syscall::signalable::resolve_signal_target(
        ctx.task().flags,
        target_id,
    )?;
    if task_terminate(target.id()) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
});

define_syscall!(syscall_exec
    (ctx, path_ptr: u64, argv_ptr: u64, envp_ptr: u64)
    cap(NoneSelf)
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

            // Point of no return: the old image is gone.
            let task_id = ctx.task_id();

            // Authority becomes `grant(image) & held`. Both halves matter:
            // intersecting is what stops a privileged program's authority
            // surviving into an arbitrary binary (an entitlement outliving
            // exec is one of the two CVE shapes this model is built against),
            // and taking the grant at all is what lets a launcher hand a
            // program the authority its *identity* earns.
            //
            // Reduction is total and infallible -- no error return a caller
            // could ignore -- because a historical local root came from an
            // attacker making a privilege drop fail inside a program that
            // ignored the result.
            //
            // Here rather than in `do_exec`: this is the point of no return,
            // past every fallible step and before the new image's first
            // instruction.
            {
                let (granted_flags, _) = exec::grants::grant_for(path);
                let granted = slopos_ostd::authority::caps_from_task_flags(
                    granted_flags | slopos_abi::task::TASK_FLAG_USER_MODE,
                );
                let before = slopos_ostd::task::ops::task_caps(ctx.task());
                let after =
                    slopos_ostd::task::ops::task_restrict_caps(ctx.task(), granted);
                if after != before {
                    slopos_ostd::klog_info!(
                        "exec: task {} authority {:#x} -> {:#x}",
                        task_id,
                        before,
                        after,
                    );
                }
            }

            slopos_sched::task::task_cleanup_for_exec(task_id);

            // SIG_DFL so no stale handler pointer survives into the new image;
            // SIG_IGN and blocked/pending state stay (POSIX exec semantics).
            if let Some(task) = Some(ctx.task()) {
                task_reset_caught_handlers(task);
            }

            if tls_tp != 0 {
                {
        let t = ctx.task();
                    t.set_fs_base(tls_tp);
                }
                slopos_arch::cpu::msr::write_msr(slopos_arch::cpu::msr::Msr::FS_BASE, tls_tp);
            }
            let uc = ctx.user_ctx();
            let mut regs = uc.regs();
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

            // The new image must never see the previous program's vector
            // registers. Reset and load the default under IRQ-off, so a context
            // switch cannot re-save the old live registers over the reset.
            let xcr0 = slopos_ostd::cpu::x86_64::xsave::active_xcr0();
            slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
                if let Some(current) = slopos_sched::task_struct::Current::get() {
                    current.task().fpu_reset(&current);
                    let restored = current.task().fpu_restore_to_cpu(&current, xcr0);
                    debug_assert!(restored, "XRSTOR64 rejected the FPU init image");
                }
            });
            SyscallResult::NoReturn
        }
        Err(e) => SyscallResult::Err(Errno::from_raw(e as i32).unwrap_or(Errno::EINVAL)),
    }
});

define_syscall!(syscall_get_cpu_count (ctx) cap(NoneSelf)
    -> Result<u64, Errno> {
    Ok(slopos_arch::pcr::get_cpu_count() as u64)
});

define_syscall!(syscall_get_current_cpu (ctx) cap(NoneSelf)
    -> Result<u64, Errno> {
    Ok(slopos_arch::pcr::get_current_cpu() as u64)
});

define_syscall!(syscall_set_cpu_affinity
    (ctx, target: u32, new_affinity: u32)
    cap(NoneRelation)
    requires(let task_id: task_id, let process_id: process_id)
    -> Result<(), Errno>
{
    let resolved = if target == 0 { task_id } else { target };
    let Some(task_ref) = task_find_by_id(resolved) else {
        return Err(Errno::ESRCH);
    };
    // Unchecked, any task could pin a `NO_PREEMPT` spinner per CPU and wedge
    // every core, so pinning is confined to a shared address space. Compared as
    // tables, not numbers — a recycled id would let a *later* process pass.
    if task_ref.process().as_deref().and_then(FdTable::of) != Some(process_id) {
        return Err(Errno::EPERM);
    }
    task_ref.set_cpu_affinity(new_affinity);
    // Stamping the mask is not enough — re-place the task so the new mask
    // actually governs where it runs.
    task_apply_affinity(&task_ref, new_affinity);
    Ok(())
});

define_syscall!(syscall_get_cpu_affinity
    (ctx, target: u32)
    cap(NoneRelation)
    requires(let task_id: task_id, let process_id: process_id)
    -> Result<u64, Errno>
{
    let resolved = if target == 0 { task_id } else { target };
    let Some(task_ref) = task_find_by_id(resolved) else {
        return Err(Errno::ESRCH);
    };
    if task_ref.process().as_deref().and_then(FdTable::of) != Some(process_id) {
        return Err(Errno::EPERM);
    }
    Ok(task_ref.cpu_affinity() as u64)
});

define_syscall!(syscall_getpid (ctx)
    cap(NoneSelf)
    requires(let task_id: task_id)
    -> Result<u32, Errno>
{
    Ok(task_id)
});

define_syscall!(syscall_getppid (ctx) cap(NoneSelf)
    -> Result<u32, Errno> {
    let task = ctx.task();
    Ok(task.parent_task_id())
});

define_syscall!(syscall_getpgid
    (ctx, target: u32)
    cap(NoneRelation)
    requires(let task_id: task_id)
    -> Result<u32, Errno>
{
    let resolved = if target == 0 { task_id } else { target };
    let Some(task_ref) = task_find_by_id(resolved) else {
        return Err(Errno::ESRCH);
    };
    Ok(task_ref.pgid())
});

define_syscall!(syscall_setpgid
    (ctx, pid: u32, pgid_arg: u32)
    cap(NoneRelation)
    requires(let task_id: task_id)
    -> Result<(), Errno>
{
    let resolved_pid = if pid == 0 { task_id } else { pid };
    let resolved_pgid = if pgid_arg == 0 { resolved_pid } else { pgid_arg };

    let Some(target_ref) = task_find_by_id(resolved_pid) else {
        return Err(Errno::EINVAL);
    };
    if resolved_pgid == 0 {
        return Err(Errno::EINVAL);
    }

    let caller_sid = ctx.task().sid();
    let target = &*target_ref;
    if resolved_pid != task_id && target.parent_task_id() != task_id {
        return Err(Errno::EINVAL);
    }
    if target.sid() != caller_sid {
        return Err(Errno::EINVAL);
    }

    // The group object must mirror the integer pgid the target is about to hold.
    let new_group = if resolved_pgid == resolved_pid {
        match target.process_group.load() {
            Some(existing) if existing.id() == resolved_pgid => Some(existing),
            _ => {
                let session = target
                    .process_group
                    .load()
                    .map(|pg| pg.session().clone())
                    .ok_or(Errno::EPERM)?;
                Some(new_group_in_session(resolved_pgid, session).ok_or(Errno::ENOMEM)?)
            }
        }
    } else {
        let Some(leader_ref) = task_find_by_id(resolved_pgid) else {
            return Err(Errno::EINVAL);
        };
        if leader_ref.sid() != caller_sid {
            return Err(Errno::EINVAL);
        }
        Some(leader_ref.process_group.load().ok_or(Errno::EINVAL)?)
    };

    // `target` is generally *not* the calling task, so another CPU may be
    // reading these fields. Integer first, membership second: the slot's
    // Release store orders the pair and defers the displaced handle's release
    // past any concurrent reader's clone.
    target.set_pgid(resolved_pgid);
    target.process_group.store(new_group);
    Ok(())
});

define_syscall!(syscall_setsid (ctx)
    cap(NoneSelf)
    requires(let task_id: task_id)
    -> Result<u32, Errno>
{
    let task = ctx.task();
    if task.pgid() == task.task_id || task.sid() == task.task_id {
        return Err(Errno::EPERM);
    }
    // Installing the fresh group drops the old membership, so the old session
    // and any terminal weak links to it die with it.
    let pg = new_session_group(task.task_id).ok_or(Errno::ENOMEM)?;
    if task.controlling_tty().is_some() {
        task.set_controlling_tty(None);
    }
    // Integers first, membership second — see `syscall_setpgid`.
    task.set_sid(task.task_id);
    task.set_pgid(task.task_id);
    task.process_group.store(Some(pg));
    Ok(task.sid())
});

define_syscall!(syscall_getuid (ctx) cap(NoneSelf)
    -> u32 { 0 });
define_syscall!(syscall_getgid (ctx) cap(NoneSelf)
    -> u32 { 0 });
define_syscall!(syscall_geteuid (ctx) cap(NoneSelf)
    -> u32 { 0 });
define_syscall!(syscall_getegid (ctx) cap(NoneSelf)
    -> u32 { 0 });

define_syscall!(syscall_chdir
    (ctx, path: UserCStr<USER_PATH_MAX>) cap(NoneSelf)
    -> Result<(), Errno>
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

    let current = Current::get().ok_or(Errno::EINVAL)?;
    if !current.task().set_cwd(&current, path.as_bytes()) {
        return Err(Errno::ENAMETOOLONG);
    }
    Ok(())
});

define_syscall!(syscall_getcwd
    (ctx, buf: UserBytes) cap(NoneSelf)
    -> Result<u64, Errno>
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
    (ctx, cmd: u64, addr: u64) cap(NoneSelf)
    -> Result<(), Errno>
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

define_syscall!(syscall_fork (ctx) cap(NoneSelf)
    -> Result<u64, Errno> {
    let task = ctx.task();
    let child_id = task_fork(task, Some(ctx.user_ctx()));
    if child_id == slopos_abi::task::INVALID_TASK_ID {
        Err(Errno::EAGAIN)
    } else {
        Ok(child_id as u64)
    }
});

define_syscall!(syscall_clone
    (ctx, flags: u64, child_stack: u64, parent_tidptr: u64, child_tidptr: u64, tls: u64)
    cap(NoneSelf)
    -> Result<u64, Errno>
{
    let parent = ctx.task();
    match slopos_sched::task::task_clone(
        parent,
        Some(ctx.user_ctx()),
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
    (ctx, uaddr: u64, op: u64, val: u32, timeout: u64) cap(NoneSelf)
    -> Result<u64, Errno>
{
    if (uaddr & 0x3) != 0 {
        return Err(Errno::EINVAL);
    }

    let user_word = MmUserPtr::<u32>::try_new(uaddr).map_err(|_| Errno::EFAULT)?;
    if copy_from_user(user_word).is_err() {
        return Err(Errno::EFAULT);
    }

    let rc = match op {
        // 0 means no timeout, per the syscall's documented contract.
        FUTEX_WAIT => {
            let deadline = if timeout == 0 { None } else { Some(timeout) };
            slopos_sched::futex::futex_wait(uaddr, val, deadline)
        }
        FUTEX_WAKE => slopos_sched::futex::futex_wake(uaddr, val),
        _ => ENOSYS_RETURN as i64,
    };

    Ok(rc as u64)
});

define_syscall!(syscall_vhangup (ctx)
    cap(NoneRelation)
    requires(let task_id: task_id)
    -> Result<(), Errno>
{
    let ctty = match ctx.task().controlling_tty() {
        Some(idx) => idx,
        None => return Err(Errno::EPERM),
    };
    slopos_kernel_services::syscall_services::tty::hangup(ctty);
    Ok(())
});

#[allow(dead_code)]
type _Unused<T> = UserPtr<T>;

define_syscall!(syscall_prlimit64
    (ctx, pid: u32, resource: u32, new_ptr: u64, old_ptr: u64)
    cap(NoneRelation)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    use slopos_abi::quota::{RLIM64_INFINITY, RLimit64, rlimit_mapping};
    use slopos_ostd::process::quota::{KindStats, NO_LIMIT, set_limit, stats};

    let process = process_id.process().ok_or(Errno::ESRCH)?;
    // Self only: there is no privilege principal in this kernel (getuid returns
    // a literal 0), so a cross-process limit change has no answer but
    // "everyone may".
    if pid != 0 && pid != process.id() {
        return Err(Errno::EPERM);
    }

    let mapping = rlimit_mapping(resource).ok_or(Errno::EINVAL)?;
    let account = process.account();
    // An account row reaped mid-call reports the enforced default rather than
    // failing: `ESRCH` here would make the syscall depend on reap timing.
    let current = stats(account, mapping.kind).unwrap_or(KindStats {
        used: 0,
        limit: slopos_abi::quota::default_process_limit(mapping.kind),
        peak: 0,
        denials: 0,
    });
    let publish = |limit: u32| -> u64 {
        if limit == NO_LIMIT {
            RLIM64_INFINITY
        } else {
            (limit as u64).saturating_mul(mapping.scale)
        }
    };

    // Read before write, so a call that both queries and sets reports what was
    // in force when it was made rather than what it just installed.
    if old_ptr != 0 {
        let out = MmUserPtr::<RLimit64>::try_new(old_ptr).map_err(|_| Errno::EFAULT)?;
        // Soft and hard are the same number: there is no privileged path to
        // raise one above the other, so reporting them apart would imply
        // headroom that cannot be claimed.
        let value = publish(current.limit);
        copy_to_user(out, &RLimit64 { rlim_cur: value, rlim_max: value })
            .map_err(|_| Errno::EFAULT)?;
    }

    if new_ptr != 0 {
        let src = MmUserPtr::<RLimit64>::try_new(new_ptr).map_err(|_| Errno::EFAULT)?;
        let want = copy_from_user(src).map_err(|_| Errno::EFAULT)?;
        if want.rlim_cur > want.rlim_max {
            return Err(Errno::EINVAL);
        }
        // Lowering only: raising the ceiling is the privileged operation, and
        // granting it unconditionally would make every limit advisory.
        if want.rlim_max > publish(current.limit) {
            return Err(Errno::EPERM);
        }
        // Saturating, never `NO_LIMIT`: mapping an over-wide `rlim_cur` to the
        // no-limit sentinel would turn the widest possible *set* into a way to
        // switch enforcement off.
        let scaled = want.rlim_cur / mapping.scale.max(1);
        let requested = u32::try_from(scaled).unwrap_or(u32::MAX).min(current.limit);
        set_limit(account, mapping.kind, requested);
    }

    Ok(())
});
