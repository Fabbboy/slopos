use alloc::vec::Vec;

use crate::exec;
use crate::sched::task_wait_for;
use crate::scheduler::task::{task_find_by_id, task_fork, task_terminate};
use crate::scheduler::task_struct::Task;
use crate::syscall::common::{
    SyscallDisposition, syscall_bounded_from_user, syscall_copy_to_user_bounded,
    syscall_copy_user_str, syscall_return_err,
};
use crate::syscall::context::SyscallContext;
use slopos_abi::fs::FS_TYPE_DIRECTORY;
use slopos_abi::syscall::*;
use slopos_abi::task::{INVALID_TASK_ID, TaskExitRecord};
use slopos_fs::vfs::traits::VfsError;

use slopos_lib::InterruptFrame;
use slopos_mm::user_copy::{copy_from_user, copy_to_user};
use slopos_mm::user_ptr::UserPtr;

use crate::task::task_get_exit_record;

fn read_user_ptr_array_terminated(base_ptr: u64, max_count: usize) -> Result<Vec<u64>, ()> {
    let mut out = Vec::new();
    if out.try_reserve(max_count).is_err() {
        return Err(());
    }

    for idx in 0..max_count {
        let slot_addr = base_ptr
            .checked_add((idx * core::mem::size_of::<u64>()) as u64)
            .ok_or(())?;
        let user_slot = UserPtr::<u64>::try_new(slot_addr).map_err(|_| ())?;
        let value = copy_from_user(user_slot).map_err(|_| ())?;
        if value == 0 {
            return Ok(out);
        }
        out.push(value);
    }

    Err(())
}

fn read_user_ptr_array_count(
    base_ptr: u64,
    count: usize,
    max_count: usize,
) -> Result<Vec<u64>, ()> {
    if count > max_count {
        return Err(());
    }

    let mut out = Vec::new();
    if out.try_reserve(count).is_err() {
        return Err(());
    }

    for idx in 0..count {
        let slot_addr = base_ptr
            .checked_add((idx * core::mem::size_of::<u64>()) as u64)
            .ok_or(())?;
        let user_slot = UserPtr::<u64>::try_new(slot_addr).map_err(|_| ())?;
        let value = copy_from_user(user_slot).map_err(|_| ())?;
        if value == 0 {
            break;
        }
        out.push(value);
    }

    Ok(out)
}

fn read_user_cstr_list(ptrs: &[u64]) -> Result<Vec<Vec<u8>>, ()> {
    let mut out = Vec::new();
    if out.try_reserve(ptrs.len()).is_err() {
        return Err(());
    }

    for &ptr in ptrs {
        let mut buf = [0u8; exec::EXEC_MAX_ARG_STRLEN];
        syscall_copy_user_str(&mut buf, ptr).map_err(|_| ())?;
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());

        let mut s = Vec::new();
        if s.try_reserve(len).is_err() {
            return Err(());
        }
        s.extend_from_slice(&buf[..len]);
        out.push(s);
    }

    Ok(out)
}

define_syscall!(syscall_spawn_path(ctx, args) {
    let path_ptr = args.arg0 as *const u8;
    let path_len = args.arg1 as usize;
    let priority = args.arg2 as u8;
    let flags = args.arg3 as u16;
    let argv_ptr = args.arg4;
    let argc = args.arg5 as usize;

    if path_ptr.is_null() || path_len == 0 || path_len > exec::EXEC_MAX_PATH {
        return ctx.err();
    }

    let mut path_buf = [0u8; exec::EXEC_MAX_PATH];
    let copied_len = match syscall_bounded_from_user(
        &mut path_buf,
        path_ptr as u64,
        path_len as u64,
        exec::EXEC_MAX_PATH,
    ) {
        Ok(len) => len,
        Err(_) => {
            return ctx.err();
        }
    };

    let argv_storage = if argv_ptr != 0 && argc > 0 {
        let argv_ptrs = match read_user_ptr_array_count(argv_ptr, argc, exec::EXEC_MAX_ARGS) {
            Ok(ptrs) => ptrs,
            Err(_) => return ctx.err(),
        };
        match read_user_cstr_list(argv_ptrs.as_slice()) {
            Ok(values) => Some(values),
            Err(_) => return ctx.err(),
        }
    } else {
        None
    };

    let argv_refs = argv_storage
        .as_ref()
        .map(|values| values.iter().map(|v| v.as_slice()).collect::<Vec<&[u8]>>());

    let parent_pid = ctx.process_id().unwrap_or(slopos_abi::task::INVALID_PROCESS_ID);
    let parent_tid = ctx.task_id().unwrap_or(slopos_abi::task::INVALID_TASK_ID);
    match exec::spawn_program_with_attrs(
        &path_buf[..copied_len],
        argv_refs.as_deref(),
        priority,
        flags,
        parent_pid,
        parent_tid,
    ) {
        Ok(task_id) => ctx.ok(task_id as u64),
        Err(err) => ctx.ok(err as i32 as u64),
    }
});

define_syscall!(syscall_waitpid(ctx, args) {
    let target_id = args.arg0 as u32;
    let options = args.arg1 as u32;
    if target_id == 0 || target_id == INVALID_TASK_ID {
        return ctx.err();
    }

    let mut record = TaskExitRecord::empty();
    if task_get_exit_record(target_id, &mut record) == 0 {
        return ctx.ok(record.exit_code as u64);
    }

    if (options & 0x1) != 0 {
        return ctx.err_with(ERRNO_EAGAIN);
    }

    task_wait_for(target_id);

    let mut record2 = TaskExitRecord::empty();
    if task_get_exit_record(target_id, &mut record2) == 0 {
        ctx.ok(record2.exit_code as u64)
    } else {
        ctx.ok(0)
    }
});

define_syscall!(syscall_terminate_task(ctx, args) requires(compositor) {
    let target_id = args.arg0_u32();
    if target_id == 0 || target_id == INVALID_TASK_ID {
        return ctx.err();
    }

    let caller_id = ctx.task_id().unwrap_or(INVALID_TASK_ID);
    if target_id == caller_id {
        return ctx.err();
    }

    if task_terminate(target_id) != 0 {
        return ctx.err();
    }

    ctx.ok(0)
});

pub fn syscall_exec(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, ERRNO_EINVAL);
    };

    let process_id = match ctx.require_process_id() {
        Ok(id) => id,
        Err(d) => return d,
    };

    let args = ctx.args();
    let path_ptr = args.arg0;
    let argv_ptr = args.arg1;
    let envp_ptr = args.arg2;

    if path_ptr == 0 {
        return ctx.err();
    }

    let mut path_buf = [0u8; exec::EXEC_MAX_PATH];
    if syscall_copy_user_str(&mut path_buf, path_ptr).is_err() {
        return ctx.err();
    }

    let path_len = path_buf
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(path_buf.len());
    let path = &path_buf[..path_len];

    let argv_storage = if argv_ptr != 0 {
        let argv_ptrs = match read_user_ptr_array_terminated(argv_ptr, exec::EXEC_MAX_ARGS) {
            Ok(ptrs) => ptrs,
            Err(_) => return ctx.err(),
        };
        match read_user_cstr_list(argv_ptrs.as_slice()) {
            Ok(values) => Some(values),
            Err(_) => return ctx.err(),
        }
    } else {
        None
    };

    let envp_storage = if envp_ptr != 0 {
        let envp_ptrs = match read_user_ptr_array_terminated(envp_ptr, exec::EXEC_MAX_ENVS) {
            Ok(ptrs) => ptrs,
            Err(_) => return ctx.err(),
        };
        match read_user_cstr_list(envp_ptrs.as_slice()) {
            Ok(values) => Some(values),
            Err(_) => return ctx.err(),
        }
    } else {
        None
    };

    let argv_refs = argv_storage
        .as_ref()
        .map(|values| values.iter().map(|v| v.as_slice()).collect::<Vec<&[u8]>>());
    let envp_refs = envp_storage
        .as_ref()
        .map(|values| values.iter().map(|v| v.as_slice()).collect::<Vec<&[u8]>>());

    let mut entry_point = 0u64;
    let mut stack_ptr = 0u64;

    match exec::do_exec(
        process_id,
        path,
        argv_refs.as_deref(),
        envp_refs.as_deref(),
        &mut entry_point,
        &mut stack_ptr,
    ) {
        Ok(()) => {
            // Point of no return: old image is gone.  Tear down task-bound
            // resources (compositor surface, shm buffers, input queues, …)
            // so the new program can register fresh ones.
            let task_id = unsafe { (*task).task_id };
            crate::scheduler::task::task_cleanup_for_exec(task_id);

            unsafe {
                (*frame).rip = entry_point;
                (*frame).rsp = stack_ptr;
                (*frame).rax = 0;
                (*frame).rdi = 0;
                (*frame).rsi = 0;
                (*frame).rdx = 0;
                (*frame).rcx = 0;
                (*frame).r8 = 0;
                (*frame).r9 = 0;
                (*frame).r10 = 0;
                (*frame).r11 = 0;
            }
            SyscallDisposition::Ok
        }
        Err(e) => {
            unsafe {
                (*frame).rax = e as i32 as u64;
            }
            SyscallDisposition::Ok
        }
    }
}

define_syscall!(syscall_get_cpu_count(ctx, args) {
    let _ = args;
    ctx.ok(slopos_lib::get_cpu_count() as u64)
});

define_syscall!(syscall_get_current_cpu(ctx, args) {
    let _ = args;
    ctx.ok(slopos_lib::get_current_cpu() as u64)
});

define_syscall!(syscall_set_cpu_affinity(ctx, args) requires(let task_id) {
    let target_or_zero = args.arg0_u32();
    let new_affinity = args.arg1_u32();
    let resolved_task_id = if target_or_zero == 0 { task_id } else { target_or_zero };

    let task_ptr = task_find_by_id(resolved_task_id);
    if task_ptr.is_null() {
        return ctx.err();
    }

    unsafe { (*task_ptr).cpu_affinity = new_affinity; }
    ctx.ok(0)
});

define_syscall!(syscall_get_cpu_affinity(ctx, args) requires(let task_id) {
    let target_or_zero = args.arg0_u32();
    let resolved_task_id = if target_or_zero == 0 { task_id } else { target_or_zero };

    let task_ptr = task_find_by_id(resolved_task_id);
    if task_ptr.is_null() {
        return ctx.err();
    }

    ctx.ok(unsafe { (*task_ptr).cpu_affinity } as u64)
});

define_syscall!(syscall_getpid(ctx, args) requires(let task_id) {
    let _ = args;
    ctx.ok(task_id as u64)
});

define_syscall!(syscall_getppid(ctx, args) {
    let _ = args;
    let task = some_or_err!(ctx, ctx.task_mut());
    ctx.ok(task.parent_task_id as u64)
});

define_syscall!(syscall_getpgid(ctx, args) requires(let task_id) {
    let target = args.arg0_u32();
    let resolved = if target == 0 { task_id } else { target };
    let task_ptr = task_find_by_id(resolved);
    if task_ptr.is_null() {
        return ctx.err();
    }
    ctx.ok(unsafe { (*task_ptr).pgid } as u64)
});

define_syscall!(syscall_setpgid(ctx, args) requires(let task_id) {
    let pid = args.arg0_u32();
    let pgid_arg = args.arg1_u32();
    let resolved_pid = if pid == 0 { task_id } else { pid };
    let resolved_pgid = if pgid_arg == 0 { resolved_pid } else { pgid_arg };

    let caller_ptr = task_find_by_id(task_id);
    let target_ptr = task_find_by_id(resolved_pid);
    if caller_ptr.is_null() || target_ptr.is_null() || resolved_pgid == 0 {
        return ctx.err();
    }

    let caller = unsafe { &*caller_ptr };
    let target = unsafe { &mut *target_ptr };
    if resolved_pid != task_id && target.parent_task_id != task_id {
        return ctx.err();
    }
    if target.sid != caller.sid {
        return ctx.err();
    }

    if resolved_pgid != resolved_pid {
        let leader_ptr = task_find_by_id(resolved_pgid);
        if leader_ptr.is_null() {
            return ctx.err();
        }
        let leader = unsafe { &*leader_ptr };
        if leader.sid != caller.sid {
            return ctx.err();
        }
    }

    target.pgid = resolved_pgid;
    ctx.ok(0)
});

define_syscall!(syscall_setsid(ctx, args) requires(let task_id) {
    let _ = args;
    let task_ptr = task_find_by_id(task_id);
    if task_ptr.is_null() {
        return ctx.err();
    }
    let task = unsafe { &mut *task_ptr };
    // POSIX: EPERM if the process is already a process group leader
    // or already a session leader.
    if task.pgid == task.task_id || task.sid == task.task_id {
        return ctx.err();
    }
    // Clear any inherited controlling terminal. Since we already rejected
    // session leaders above, the caller is never their session's leader,
    // so just drop the reference without a full release.
    if task.controlling_tty.is_some() {
        task.controlling_tty = None;
    }
    // Create new session: task becomes session leader and pgrp leader.
    task.sid = task.task_id;
    task.pgid = task.task_id;
    ctx.ok(task.sid as u64)
});

define_syscall!(syscall_getuid(ctx, args) {
    let _ = args;
    ctx.ok(0)
});

define_syscall!(syscall_getgid(ctx, args) {
    let _ = args;
    ctx.ok(0)
});

define_syscall!(syscall_geteuid(ctx, args) {
    let _ = args;
    ctx.ok(0)
});

define_syscall!(syscall_getegid(ctx, args) {
    let _ = args;
    ctx.ok(0)
});

define_syscall!(syscall_chdir(ctx, args) {
    let path_ptr = args.arg0;
    if path_ptr == 0 {
        return ctx.bad_address();
    }

    let mut path_buf = [0u8; 256];
    if syscall_copy_user_str(&mut path_buf, path_ptr).is_err() {
        return ctx.bad_address();
    }

    let path_len = path_buf
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(path_buf.len());
    if path_len == 0 {
        return ctx.err_with(ERRNO_EINVAL);
    }

    let path = &path_buf[..path_len];
    match slopos_fs::vfs::ops::vfs_stat(path) {
        Ok((file_type, _size)) => {
            if file_type != FS_TYPE_DIRECTORY {
                return ctx.err_with(ERRNO_ENOTDIR);
            }
        }
        Err(VfsError::NotDirectory) => return ctx.err_with(ERRNO_ENOTDIR),
        Err(VfsError::InvalidPath) => return ctx.err_with(ERRNO_EINVAL),
        Err(_) => return ctx.err_with(ERRNO_ENOENT),
    }

    let task = some_or_err!(ctx, ctx.task_mut());
    task.cwd[..=path_len].copy_from_slice(&path_buf[..=path_len]);
    task.cwd_len = path_len as u16;

    ctx.ok(0)
});

define_syscall!(syscall_getcwd(ctx, args) {
    let buf_ptr = args.arg0;
    let buf_size = args.arg1 as usize;

    if buf_ptr == 0 {
        return ctx.bad_address();
    }

    let task = some_or_err!(ctx, ctx.task_mut());
    let cwd_len = task.cwd_len as usize;
    let needed = cwd_len + 1;

    if buf_size < needed {
        return ctx.err_with(ERRNO_ERANGE);
    }

    if syscall_copy_to_user_bounded(buf_ptr, &task.cwd[..needed]).is_err() {
        return ctx.bad_address();
    }

    ctx.ok(needed as u64)
});

pub fn syscall_arch_prctl(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, ERRNO_EINVAL);
    };

    let args = ctx.args();
    let cmd = args.arg0;
    let addr = args.arg1;

    match cmd {
        ARCH_SET_FS => {
            if addr >= slopos_mm::memory_layout_defs::USER_SPACE_END_VA && addr != 0 {
                return ctx.invalid_arg();
            }
            let t = some_or_err!(ctx, ctx.task_mut());
            t.fs_base = addr;
            slopos_lib::cpu::msr::write_msr(slopos_lib::cpu::msr::Msr::FS_BASE, addr);
            ctx.ok(0)
        }
        ARCH_GET_FS => {
            if addr == 0 {
                return ctx.invalid_arg();
            }
            let t = some_or_err!(ctx, ctx.task_mut());
            let fs_base_val = t.fs_base;
            let user_ptr = try_or_err!(ctx, UserPtr::<u64>::try_new(addr));
            try_or_err!(ctx, copy_to_user(user_ptr, &fs_base_val));
            ctx.ok(0)
        }
        _ => ctx.invalid_arg(),
    }
}

pub fn syscall_fork(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, ERRNO_EINVAL);
    };

    let child_id = task_fork(task, frame as *const InterruptFrame);
    ctx.from_bool_value(
        child_id != slopos_abi::task::INVALID_TASK_ID,
        child_id as u64,
    )
}

pub fn syscall_clone(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, ERRNO_EINVAL);
    };

    let args = ctx.args();
    let flags = args.arg0;
    let child_stack = args.arg1;
    let parent_tidptr = args.arg2;
    let child_tidptr = args.arg3;
    let tls = args.arg4;

    match crate::scheduler::task::task_clone(
        task,
        flags,
        child_stack,
        parent_tidptr,
        child_tidptr,
        tls,
    ) {
        Ok(child_id) => ctx.ok(child_id as u64),
        Err(errno) => syscall_return_err(frame, errno),
    }
}

pub fn syscall_futex(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, ERRNO_EINVAL);
    };

    let args = ctx.args();
    let uaddr = args.arg0;
    let op = args.arg1;
    let val = args.arg2_u32();
    let timeout = args.arg3;

    if (uaddr & 0x3) != 0 {
        return ctx.invalid_arg();
    }

    let user_word = match UserPtr::<u32>::try_new(uaddr) {
        Ok(p) => p,
        Err(_) => return ctx.bad_address(),
    };
    if copy_from_user(user_word).is_err() {
        return ctx.bad_address();
    }

    let rc = match op {
        FUTEX_WAIT => crate::scheduler::futex::futex_wait(uaddr, val, timeout),
        FUTEX_WAKE => crate::scheduler::futex::futex_wake(uaddr, val),
        _ => ENOSYS_RETURN as i64,
    };

    ctx.ok(rc as u64)
}
