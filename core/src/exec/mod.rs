//! exec() syscall implementation for loading and executing ELF binaries from filesystem.

#[cfg(feature = "test-hooks")]
pub mod tests;
#[cfg(feature = "test-hooks")]
pub mod utest;

use core::ffi::{c_char, c_int};
use core::ptr;

use slopos_abi::Errno;
use slopos_ostd::KVec;

use slopos_abi::auxv::{AT_ENTRY, AT_NULL, AT_PAGESZ, AT_PHDR, AT_PHENT, AT_PHNUM};
use slopos_abi::task::{
    INVALID_PROCESS_ID, TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE, TASK_NAME_MAX_LEN, TaskPriority,
};
use slopos_fs::fileio::{
    FileRef, file_close_fd, fileio_clone_file_ref, fileio_create_empty_table_for_process,
    fileio_destroy_table_for_process, fileio_install_file_ref_at, fileio_open_at_fd,
    fileio_take_file_ref_matching,
};
use slopos_fs::vfs::ops::vfs_open;
use slopos_mm::elf::{ElfError, ElfExecInfo};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_mm::process_vm::{
    process_vm_get_page_dir, process_vm_get_stack_top, process_vm_get_vm_space,
    process_vm_load_elf_data, process_vm_reset_stack, process_vm_write_user_bytes,
};
use slopos_ostd::klog_info;

use slopos_abi::task::INVALID_TASK_ID;
use slopos_ostd::task::new_group_in_session;
use slopos_sched::scheduler::publish_new_task;
use slopos_sched::task::{TaskEntry, task_default_signals_in_mask, task_entry_from_kernel_va};
use slopos_sched::task::{
    link_child, task_abandon, task_build, task_commit, task_find_by_id, task_terminate,
};

pub const EXEC_MAX_PATH: usize = 256;
pub const EXEC_MAX_ARG_STRLEN: usize = 4096;
pub const EXEC_MAX_ARGS: usize = 32;
pub const EXEC_MAX_ENVS: usize = 32;
pub const EXEC_MAX_ELF_SIZE: usize = 16 * 1024 * 1024;

pub const INIT_PATH: &[u8] = b"/sbin/init";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExecError {
    NoEntry = -2,
    IoError = -5,
    TooManyArgs = -7,
    NoExec = -8,
    BadFd = -9,
    NoMem = -12,
    Fault = -14,
    NameTooLong = -36,
}

/// A decoded spawn file action (kernel-owned; `Open` paths are copied from
/// user memory in the syscall handler before crossing into `exec`).
pub enum FdAction {
    /// Share the parent's `src_fd` description into the child's `target_fd`.
    Clone { src_fd: i32, target_fd: i32 },
    /// Move the parent's `src_fd` into the child's `target_fd`.
    Transfer { src_fd: i32, target_fd: i32 },
    /// Close the child's `target_fd`.
    Close { target_fd: i32 },
    /// Open `path` into the child's `target_fd`.
    Open {
        target_fd: i32,
        path: KVec<u8>,
        flags: u32,
    },
}

impl From<ElfError> for ExecError {
    fn from(_: ElfError) -> Self {
        ExecError::NoExec
    }
}

fn trim_nul_bytes(bytes: &[u8]) -> &[u8] {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    &bytes[..len]
}

pub fn launch_init() -> Result<u32, ExecError> {
    spawn_program_with_attrs(
        INIT_PATH,
        None,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE | TASK_FLAG_SYSTEM,
        &[],
        0,
        INVALID_PROCESS_ID,
        INVALID_TASK_ID,
    )
}

/// Apply the spawn fd-action allow-list to the child's empty, unpublished
/// table. `Clone`/`Transfer` resolve against the parent; `Open` opens a path;
/// each installs at an explicit child fd. Any failure aborts with the parent
/// table untouched: `Transfer` installs a shared alias first and empties its
/// parent slot only after the whole list has applied, so the caller tears the
/// child down (all-or-nothing) while the parent keeps every descriptor.
pub(crate) fn apply_fd_actions(
    parent_process_id: u32,
    child_process_id: u32,
    actions: &[FdAction],
) -> Result<(), ExecError> {
    let mut transfers: KVec<(i32, FileRef)> =
        KVec::with_capacity(actions.len()).map_err(|_| ExecError::NoMem)?;
    for action in actions {
        let rc: c_int = match action {
            FdAction::Clone { src_fd, target_fd } => {
                match fileio_clone_file_ref(parent_process_id, *src_fd) {
                    Some(file) => {
                        fileio_install_file_ref_at(child_process_id, *target_fd, file, false)
                    }
                    None => Errno::EBADF.raw(),
                }
            }
            FdAction::Transfer { src_fd, target_fd } => {
                match fileio_clone_file_ref(parent_process_id, *src_fd) {
                    Some(file) => {
                        let moved = file.alias();
                        let rc =
                            fileio_install_file_ref_at(child_process_id, *target_fd, file, false);
                        if rc >= 0 && transfers.push((*src_fd, moved)).is_err() {
                            return Err(ExecError::NoMem);
                        }
                        rc
                    }
                    None => Errno::EBADF.raw(),
                }
            }
            FdAction::Close { target_fd } => {
                let rc = file_close_fd(child_process_id, *target_fd);
                // A fresh child table holds nothing at most fds; closing an
                // absent one is a no-op success.
                if rc == Errno::EBADF.raw() { 0 } else { rc }
            }
            FdAction::Open {
                target_fd,
                path,
                flags,
            } => fileio_open_at_fd(child_process_id, *target_fd, path.as_slice(), *flags),
        };
        if rc < 0 {
            return Err(match Errno::from_raw(rc) {
                Some(Errno::EBADF) => ExecError::BadFd,
                Some(Errno::ENOENT) => ExecError::NoEntry,
                Some(Errno::ENOMEM) => ExecError::NoMem,
                _ => ExecError::Fault,
            });
        }
    }
    // Every action applied — only now do transfers empty their parent slots.
    // The identity match skips a slot the parent concurrently closed or
    // repopulated; the taken alias drops here, lock-free.
    for (src_fd, moved) in transfers.iter() {
        drop(fileio_take_file_ref_matching(
            parent_process_id,
            *src_fd,
            moved,
        ));
    }
    Ok(())
}

fn task_name_from_path(path: &[u8]) -> Result<[u8; TASK_NAME_MAX_LEN], ExecError> {
    let trimmed = trim_nul_bytes(path);
    if trimmed.is_empty() {
        return Err(ExecError::NameTooLong);
    }

    let basename_start = trimmed
        .iter()
        .rposition(|&b| b == b'/')
        .map_or(0, |idx| idx + 1);
    let basename = &trimmed[basename_start..];

    if basename.is_empty() || basename.len() >= TASK_NAME_MAX_LEN {
        return Err(ExecError::NameTooLong);
    }

    let mut name = [0u8; TASK_NAME_MAX_LEN];
    name[..basename.len()].copy_from_slice(basename);
    Ok(name)
}

pub fn spawn_program_with_attrs(
    path: &[u8],
    argv: Option<&[&[u8]]>,
    priority: TaskPriority,
    mut flags: u16,
    actions: &[FdAction],
    sigdefault_mask: u64,
    parent_process_id: u32,
    parent_task_id: u32,
) -> Result<u32, ExecError> {
    let result = (|| {
        let normalized_path = trim_nul_bytes(path);
        if normalized_path.is_empty() || normalized_path.len() > EXEC_MAX_PATH {
            return Err(ExecError::NameTooLong);
        }

        flags |= TASK_FLAG_USER_MODE;
        let task_name = task_name_from_path(normalized_path)?;
        let user_code_entry: TaskEntry = task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64);

        // The token owns the task outright — it has no registry entry, so
        // everything below runs where no lookup, no active-task walk and no
        // other CPU can observe the task, and `task_commit` makes it reachable
        // already complete. That is what lets the field writes be a plain
        // exclusive borrow rather than an unwitnessed write into a published
        // allocation.
        let Some(mut pending) = task_build(
            task_name.as_ptr() as *const c_char,
            user_code_entry,
            ptr::null_mut(),
            priority.as_u8(),
            flags,
        ) else {
            return Err(ExecError::NoMem);
        };
        let task_id = pending.id();
        let process_id = pending.as_mut().process_id;

        let mut entry = 0u64;
        let mut stack_ptr = 0u64;
        let mut tls_tp = 0u64;

        if let Err(err) = do_exec(
            process_id,
            normalized_path,
            argv,
            None,
            &mut entry,
            &mut stack_ptr,
            &mut tls_tp,
        ) {
            task_abandon(pending);
            return Err(err);
        }

        // Build the child's fd table from the action allow-list. The child
        // starts empty; each action installs exactly what it inherits. A caller
        // with no parent process (`launch_init`) keeps its console bootstrap
        // table untouched.
        if parent_process_id != INVALID_PROCESS_ID {
            fileio_destroy_table_for_process(process_id);
            if fileio_create_empty_table_for_process(process_id) != 0 {
                task_abandon(pending);
                return Err(ExecError::NoMem);
            }
            if let Err(err) = apply_fd_actions(parent_process_id, process_id, actions) {
                task_abandon(pending);
                return Err(err);
            }
        }

        // Job control is inherited from the parent task so spawned children
        // participate in the parent's session. This matches fork semantics and
        // is required for proper job control: without it, the child is in its
        // own session and the shell cannot set it as the foreground group.
        // Resolved before the child borrow opens, so the two live at once.
        let parent_ref = task_find_by_id(parent_task_id);

        let mut fg_handoff: Option<(slopos_abi::syscall::TtyIndex, u32, u32)> = None;
        {
            let child = pending.as_mut();
            child.entry_point = entry;
            child.context.get_mut().rip = entry;
            child.context.get_mut().rsp = stack_ptr;
            child.set_fs_base(tls_tp);

            // OSTD user-mode entry: re-seed the task's `UserContext` with the
            // post-load entry / stack pointers. The kernel stack itself stays
            // as `task_build` left it (just a return-address slot pointing at
            // `user_task_first_run`); the iretq frame is rebuilt from
            // `user_ctx` on every round trip by `user_mode_round_trip_asm`.
            slopos_sched::task::init_user_ctx_for_new_task(
                child.user_ctx.get_mut(),
                entry,
                stack_ptr,
                0,
            );

            // POSIX_SPAWN_SETSIGDEF: force the named signals to their default
            // disposition in the child.
            if sigdefault_mask != 0 {
                task_default_signals_in_mask(child, sigdefault_mask);
            }

            if let Some(parent) = parent_ref.as_ref() {
                // Point the child's group object at the same identity its
                // inherited pgid names: a fresh group in the parent's session
                // for NEW_PGRP, otherwise the parent's own group. Exclusive
                // rather than an RCU store — the child is unreachable, so there
                // is no reader to defer a release past.
                let group = if flags & slopos_abi::task::TASK_FLAG_NEW_PGRP != 0 {
                    child.pgid = task_id;
                    parent
                        .process_group
                        .load()
                        .and_then(|pg| new_group_in_session(task_id, pg.session().clone()))
                } else {
                    child.pgid = parent.pgid;
                    parent.process_group.load()
                };
                let _ = child.process_group.replace_exclusive(group);
                child.sid = parent.sid;
                child.set_controlling_tty(parent.controlling_tty());

                if flags & slopos_abi::task::TASK_FLAG_FOREGROUND != 0
                    && child.pgid != 0
                    && let Some(ctty) = child.controlling_tty()
                {
                    fg_handoff = Some((ctty, child.pgid, child.sid));
                }
            }
        }

        // Every field the spawn owns is written, so the task can become
        // reachable. It is findable from here but still not runnable:
        // `publish_new_task` below is the sole schedulable edge, matching
        // Linux's wake_up_new_task() pattern.
        let Some(registered) = task_commit(pending) else {
            return Err(ExecError::NoMem);
        };

        // Publish the parent→child ownership edge (parent id + children-list
        // membership) now that the child is a live registry entry. `link_child`
        // reads the parent, and `parent_ref` is the registry guard already in
        // scope, so no pointer needs laundering back into a borrow.
        if let Some(parent) = parent_ref.as_ref()
            && let Some(child_nn) = core::ptr::NonNull::new(registered.as_ptr())
        {
            link_child(parent, child_nn);
        }

        // Atomic foreground handoff (TASK_FLAG_FOREGROUND): make the child's
        // process group the controlling terminal's foreground group *before*
        // the Ready-publish below.  Doing it parent-side after spawn returns
        // (a `tcsetpgrp` round-trip) leaves a window where the child is
        // already schedulable but still a background process — its first
        // terminal read then fails the foreground check and poisons async
        // readers.  Session-validated: a child whose session does not own the
        // terminal must not steal the foreground.
        if let Some((ctty, child_pgid, child_sid)) = fg_handoff {
            use slopos_kernel_services::syscall_services::tty;
            // The checked variant validates the session match and performs
            // the set under one TTY lock acquisition (no read-then-write
            // TOCTOU); a child whose session does not own the terminal is
            // refused. Resolving the target group walks the registry for a
            // member, which is why this follows the commit above.
            let _ = tty::set_foreground_pgrp_checked(ctty, child_pgid, child_sid);
        }

        // The Release ordering on the status store inside `publish_new_task`
        // is what makes every write above visible to the CPU that eventually
        // runs this task.
        if publish_new_task(&registered) != 0 {
            task_terminate(task_id);
            return Err(ExecError::NoMem);
        }

        Ok(task_id)
    })();

    result
}

pub fn do_exec(
    process_id: u32,
    path: &[u8],
    argv: Option<&[&[u8]]>,
    envp: Option<&[&[u8]]>,
    entry_out: &mut u64,
    stack_ptr_out: &mut u64,
    tls_tp_out: &mut u64,
) -> Result<(), ExecError> {
    if path.is_empty() || path.len() > EXEC_MAX_PATH {
        return Err(ExecError::NameTooLong);
    }

    let handle = vfs_open(path, false).map_err(|e| match e {
        slopos_fs::VfsError::NotFound => ExecError::NoEntry,
        slopos_fs::VfsError::IsDirectory => ExecError::NoExec,
        slopos_fs::VfsError::PermissionDenied => ExecError::NoExec,
        _ => ExecError::IoError,
    })?;

    let file_stat = handle
        .fs
        .stat(handle.inode)
        .map_err(|_| ExecError::IoError)?;
    if (file_stat.mode & 0o111) == 0 {
        return Err(ExecError::NoExec);
    }

    let file_size = file_stat.size as usize;
    if file_size == 0 || file_size > EXEC_MAX_ELF_SIZE {
        return Err(ExecError::NoExec);
    }

    let mut elf_data: KVec<u8> = KVec::<u8>::zeroed(file_size).map_err(|_| ExecError::NoMem)?;

    let mut offset = 0u64;
    while (offset as usize) < file_size {
        let remaining = file_size - offset as usize;
        let chunk_size = remaining.min(4096);
        let read = handle
            .read(
                offset,
                &mut elf_data[offset as usize..offset as usize + chunk_size],
            )
            .map_err(|_| ExecError::IoError)?;
        if read == 0 {
            break;
        }
        offset += read as u64;
    }

    if (offset as usize) < file_size {
        elf_data.truncate(offset as usize);
    }

    let exec_info = process_vm_load_elf_data(process_id, elf_data.as_slice(), entry_out)
        .map_err(ExecError::from)?;

    if process_vm_reset_stack(process_id) != 0 {
        return Err(ExecError::NoMem);
    }

    let stack_top = setup_user_stack(process_id, argv, envp, &exec_info)?;
    *stack_ptr_out = stack_top;
    *tls_tp_out = exec_info.tls_tp;

    // POSIX: close all FDs with FD_CLOEXEC set after point of no return.
    slopos_fs::fileio_close_on_exec(process_id);

    klog_info!(
        "exec: loaded ELF for process {}, entry={:#x}, stack={:#x}, tls_tp={:#x}",
        process_id,
        *entry_out,
        stack_top,
        *tls_tp_out,
    );

    Ok(())
}

fn setup_user_stack(
    process_id: u32,
    argv: Option<&[&[u8]]>,
    envp: Option<&[&[u8]]>,
    exec_info: &ElfExecInfo,
) -> Result<u64, ExecError> {
    let stack_top_raw = process_vm_get_stack_top(process_id);
    if stack_top_raw == 0 {
        return Err(ExecError::Fault);
    }
    let stack_top = stack_top_raw.wrapping_sub(8);

    let page_dir = process_vm_get_page_dir(process_id);
    if page_dir.is_null() {
        return Err(ExecError::NoMem);
    }
    let _ = page_dir; // legacy non-null sentinel; OSTD reads route through process_id

    let argc = argv.map(|a| a.len()).unwrap_or(0);
    let envc = envp.map(|e| e.len()).unwrap_or(0);

    if argc > EXEC_MAX_ARGS || envc > EXEC_MAX_ENVS {
        return Err(ExecError::TooManyArgs);
    }

    let mut sp = stack_top;
    sp = sp.wrapping_sub(128);
    sp &= !0xF;

    let mut string_ptrs: KVec<u64> =
        KVec::<u64>::with_capacity(argc + envc + 2).map_err(|_| ExecError::NoMem)?;

    if let Some(args) = argv {
        for arg in args.iter() {
            let len = arg.len() + 1;
            sp = sp.wrapping_sub(len as u64);
            sp &= !0x7;
            write_to_user_stack(process_id, sp, arg)?;
            write_byte_to_user_stack(process_id, sp + arg.len() as u64, 0)?;
            string_ptrs.push(sp).map_err(|_| ExecError::NoMem)?;
        }
    }

    let argv_start = string_ptrs.len();

    if let Some(envs) = envp {
        for env in envs.iter() {
            let len = env.len() + 1;
            sp = sp.wrapping_sub(len as u64);
            sp &= !0x7;
            write_to_user_stack(process_id, sp, env)?;
            write_byte_to_user_stack(process_id, sp + env.len() as u64, 0)?;
            string_ptrs.push(sp).map_err(|_| ExecError::NoMem)?;
        }
    }

    sp &= !0xF;

    // SysV ABI: rsp must be 16-byte aligned at _start with argc at [rsp].
    // Total 8-byte slots below this point: auxv (12) + envp_null (1) + envc +
    // argv_null (1) + argc_slots (argc) + argc_word (1) = argc + envc + 15.
    // If that total is odd, insert one 8-byte padding slot here (between the
    // string area and auxv) so the final sp stays 16-byte aligned.
    let total_slots = argc + envc + 15; // 12 auxv + 3 sentinel/argc
    if total_slots % 2 != 0 {
        sp = sp.wrapping_sub(8);
    }

    let auxv = [
        (AT_PHDR, exec_info.phdr_addr),
        (AT_PHENT, exec_info.phent_size as u64),
        (AT_PHNUM, exec_info.phnum as u64),
        (AT_PAGESZ, PAGE_SIZE_4KB),
        (AT_ENTRY, exec_info.entry),
        (AT_NULL, 0),
    ];
    let aux_size = auxv.len() * (2 * core::mem::size_of::<u64>());
    sp = sp.wrapping_sub(aux_size as u64);
    for (idx, (a_type, a_val)) in auxv.iter().enumerate() {
        let slot = sp + (idx as u64) * 16;
        write_u64_to_user_stack(process_id, slot, *a_type)?;
        write_u64_to_user_stack(process_id, slot + 8, *a_val)?;
    }

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(process_id, sp, 0)?;

    for i in (argv_start..string_ptrs.len()).rev() {
        sp = sp.wrapping_sub(8);
        write_u64_to_user_stack(process_id, sp, string_ptrs[i])?;
    }

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(process_id, sp, 0)?;

    for i in (0..argv_start).rev() {
        sp = sp.wrapping_sub(8);
        write_u64_to_user_stack(process_id, sp, string_ptrs[i])?;
    }

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(process_id, sp, argc as u64)?;

    Ok(sp)
}

fn write_to_user_stack(process_id: u32, addr: u64, data: &[u8]) -> Result<(), ExecError> {
    let vm_space = process_vm_get_vm_space(process_id).ok_or(ExecError::Fault)?;
    process_vm_write_user_bytes(&vm_space, addr, data).map_err(|_| ExecError::Fault)
}

fn write_byte_to_user_stack(process_id: u32, addr: u64, byte: u8) -> Result<(), ExecError> {
    write_to_user_stack(process_id, addr, &[byte])
}

fn write_u64_to_user_stack(process_id: u32, addr: u64, value: u64) -> Result<(), ExecError> {
    let bytes = value.to_le_bytes();
    write_to_user_stack(process_id, addr, &bytes)
}
