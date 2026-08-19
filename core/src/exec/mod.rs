//! exec() syscall implementation for loading and executing ELF binaries from filesystem.

pub mod grants;
#[cfg(feature = "test-hooks")]
pub mod tests;
#[cfg(feature = "test-hooks")]
pub mod utest;

use core::ffi::{c_char, c_int};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};
use slopos_fs::fileio::FdTable;

use slopos_abi::Errno;
use slopos_ostd::KVec;

use slopos_abi::auxv::{AT_ENTRY, AT_NULL, AT_PAGESZ, AT_PHDR, AT_PHENT, AT_PHNUM};
use slopos_abi::task::{TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE, TASK_NAME_MAX_LEN, TaskPriority};
use slopos_fs::fileio::{
    FileRef, file_close_fd, fileio_clone_file_ref, fileio_create_empty_table_for_process,
    fileio_destroy_table_for_process, fileio_install_file_ref_at, fileio_take_file_ref_matching,
};
use slopos_fs::vfs::ops::vfs_open;
use slopos_mm::elf::{ElfError, ElfExecInfo};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_mm::process_vm::{
    process_vm_get_stack_top, process_vm_get_vm_space, process_vm_load_elf_data,
    process_vm_reset_stack, process_vm_write_user_bytes,
};
use slopos_ostd::klog_info;

use slopos_abi::task::INVALID_TASK_ID;
use slopos_ostd::task::new_group_in_session;
use slopos_sched::scheduler::publish_new_task;
use slopos_sched::task::{SpawnGuard, link_child, task_build, task_find_by_id, task_terminate};
use slopos_sched::task::{TaskEntry, task_default_signals_in_mask, task_entry_from_kernel_va};

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

/// A decoded spawn file action; `Open` paths are already copied out of user
/// memory by the syscall handler.
pub enum FdAction {
    /// Share the parent's `src_fd` description into the child's `target_fd`.
    Clone {
        src_fd: i32,
        target_fd: i32,
    },
    /// Move the parent's `src_fd` into the child's `target_fd`.
    Transfer {
        src_fd: i32,
        target_fd: i32,
    },
    Close {
        target_fd: i32,
    },
    // `Open` is retired: it opened an arbitrary VFS path into the child with
    // no reference to what the parent held, which is endowment by *name* and
    // voids this list as an attenuating channel. Open-then-transfer is the
    // replacement and cannot exceed the spawner's own authority.
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

/// The task id `launch_init` handed to `/sbin/init`, or [`INVALID_TASK_ID`]
/// before it runs.
static INIT_TASK_ID: AtomicU32 = AtomicU32::new(INVALID_TASK_ID);

/// Which task is init. There is no structural marker — `TASK_FLAG_SYSTEM` is
/// shared with the utest runner — so the launch id names it.
pub fn init_task_id() -> u32 {
    INIT_TASK_ID.load(Ordering::Acquire)
}

pub fn launch_init() -> Result<u32, ExecError> {
    let task_id = spawn_program_with_attrs(
        INIT_PATH,
        None,
        None,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE | TASK_FLAG_SYSTEM,
        &[],
        0,
        None,
        INVALID_TASK_ID,
    )?;
    INIT_TASK_ID.store(task_id, Ordering::Release);
    Ok(task_id)
}

/// Apply the spawn fd-action list to the child's empty, unpublished table.
/// All-or-nothing: `Transfer` installs a shared alias first and empties its
/// parent slot only once the whole list applied, so any failure leaves the
/// parent holding every descriptor.
pub(crate) fn apply_fd_actions(
    parent_table: FdTable,
    child_table: FdTable,
    actions: &[FdAction],
) -> Result<(), ExecError> {
    let mut transfers: KVec<(i32, FileRef)> =
        KVec::with_capacity(actions.len()).map_err(|_| ExecError::NoMem)?;
    for action in actions {
        let rc: c_int = match action {
            FdAction::Clone { src_fd, target_fd } => {
                match fileio_clone_file_ref(parent_table, *src_fd) {
                    Some(file) => fileio_install_file_ref_at(child_table, *target_fd, file, false),
                    None => Errno::EBADF.raw(),
                }
            }
            FdAction::Transfer { src_fd, target_fd } => {
                match fileio_clone_file_ref(parent_table, *src_fd) {
                    Some(file) => {
                        let moved = file.alias();
                        let rc = fileio_install_file_ref_at(child_table, *target_fd, file, false);
                        if rc >= 0 && transfers.push((*src_fd, moved)).is_err() {
                            return Err(ExecError::NoMem);
                        }
                        rc
                    }
                    None => Errno::EBADF.raw(),
                }
            }
            FdAction::Close { target_fd } => {
                let rc = file_close_fd(child_table, *target_fd);
                // A fresh child table holds nothing at most fds; closing an absent one succeeds.
                if rc == Errno::EBADF.raw() { 0 } else { rc }
            }
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
    // The identity match skips a slot the parent concurrently closed or repopulated.
    for (src_fd, moved) in transfers.iter() {
        drop(fileio_take_file_ref_matching(parent_table, *src_fd, moved));
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

struct InheritedJobControl {
    pgid: u32,
    sid: u32,
    ctty: Option<slopos_abi::syscall::TtyIndex>,
    group: Option<slopos_ostd::KArc<slopos_ostd::task::ProcessGroup>>,
}

/// Point the child's group at the identity its inherited pgid names.
fn resolve_inherited_job_control(
    parent: &slopos_sched::task_struct::Task,
    child_task_id: u32,
    flags: u16,
) -> InheritedJobControl {
    let new_pgrp = flags & slopos_abi::task::TASK_FLAG_NEW_PGRP != 0;
    let (pgid, group) = if new_pgrp {
        let group = parent
            .process_group
            .load()
            .and_then(|pg| new_group_in_session(child_task_id, pg.session().clone()));
        (child_task_id, group)
    } else {
        (parent.pgid(), parent.process_group.load())
    };
    InheritedJobControl {
        pgid,
        sid: parent.sid(),
        ctty: parent.controlling_tty(),
        group,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_program_with_attrs(
    path: &[u8],
    argv: Option<&[&[u8]]>,
    envp: Option<&[&[u8]]>,
    mut priority: TaskPriority,
    mut flags: u16,
    actions: &[FdAction],
    sigdefault_mask: u64,
    parent_table: Option<FdTable>,
    parent_task_id: u32,
) -> Result<u32, ExecError> {
    let result = (|| {
        let normalized_path = trim_nul_bytes(path);
        if normalized_path.is_empty() || normalized_path.len() > EXEC_MAX_PATH {
            return Err(ExecError::NameTooLong);
        }

        // Privilege enters a spawn only here — the syscall boundary already
        // refused every privileged bit the caller asked for, so flags follow
        // the program, not the requester.
        let (granted_flags, granted_priority) = grants::grant_for(normalized_path);

        // ...and a spawn that *raises* authority additionally needs the
        // spawner to hold `Launch`. Without this any task obtains a privileged
        // child by spawning the privileged path, which leaks an entitlement to
        // an unrelated program without granting anything to it.
        //
        // Checked only when the grant is non-empty: an ordinary spawn raises
        // nothing and needs no right. Deliberately not an intersection with the
        // spawner's own authority — the shell holds no display authority, so
        // `spawner & grant` would mean `/bin/roulette` could never draw.
        if granted_flags != 0 {
            let spawner_may_launch = match task_find_by_id(parent_task_id) {
                Some(parent) => slopos_ostd::authority::mask_permits(
                    slopos_ostd::task::ops::task_caps(&parent),
                    slopos_ostd::authority::Capability::Launch,
                ),
                // No parent task: `launch_init`, the kernel-only root. That is
                // exactly the raise `Launch` exists to bound, performed by the
                // one caller that cannot be userland.
                None => true,
            };
            if !spawner_may_launch {
                return Err(ExecError::NoExec);
            }
        }

        flags |= granted_flags;
        if let Some(granted) = granted_priority {
            priority = granted;
        }

        flags |= TASK_FLAG_USER_MODE;
        let task_name = task_name_from_path(normalized_path)?;
        let user_code_entry: TaskEntry = task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64);

        // Unregistered and singly owned until `task_commit`, so no lookup,
        // walk or other CPU can observe the field writes below.
        let Some(pending) = task_build(
            task_name.as_ptr() as *const c_char,
            user_code_entry,
            ptr::null_mut(),
            priority.as_u8(),
            flags,
        ) else {
            return Err(ExecError::NoMem);
        };
        // Nothing else can find the orphan, so every exit from here — `?`, a
        // panic, a kill aborting the blocking calls — must release it here.
        let mut spawn = SpawnGuard::new(pending);
        let task_id = spawn.child_id();

        let mut entry = 0u64;
        let mut stack_ptr = 0u64;
        let mut tls_tp = 0u64;

        // Refused rather than defaulted: a child with no table of its own must
        // not be exec'd against the kernel's, which every kernel task shares.
        let Some(child_table) = spawn.child_table() else {
            return Err(ExecError::NoMem);
        };

        do_exec(
            child_table,
            normalized_path,
            argv,
            envp,
            &mut entry,
            &mut stack_ptr,
            &mut tls_tp,
        )?;

        // A caller with no parent process (`launch_init`) keeps its console
        // bootstrap table untouched.
        if let Some(parent_table) = parent_table {
            // Taken from the guard rather than re-resolved: the id could have
            // been returned to the allocator between the two lookups.
            let Some(FdTable::Process(child_process)) = spawn.child_table() else {
                return Err(ExecError::NoMem);
            };
            fileio_destroy_table_for_process(child_process.handle());
            if fileio_create_empty_table_for_process(child_process.handle()) != 0 {
                return Err(ExecError::NoMem);
            }
            apply_fd_actions(parent_table, FdTable::Process(child_process), actions)?;
        }

        // Inherited so the child joins the parent's session; without it the
        // shell cannot make the child its foreground group.
        let parent_ref = task_find_by_id(parent_task_id);

        // Resolved — and for NEW_PGRP allocated — before the child borrow
        // opens: that borrow runs preempt-disabled, where a fallible allocation
        // has no business.
        let inherited = parent_ref
            .as_ref()
            .map(|parent| resolve_inherited_job_control(parent, task_id, flags));

        // Authority enters here, from the program-identity grant applied
        // above -- the single raise site. Stamped on the child before it is
        // findable, so no other CPU can observe it with an unset mask.
        let child_caps = slopos_ostd::authority::caps_from_task_flags(flags);

        let Some((fg_handoff, displaced_group)) = spawn.with_child(|child| {
            slopos_ostd::task::ops::task_set_caps(child, child_caps);
            child.entry_point = entry;
            child.context.get_mut().rip = entry;
            child.context.get_mut().rsp = stack_ptr;
            child.set_fs_base(tls_tp);

            // The kernel stack stays as `task_build` left it; the iretq frame
            // is rebuilt from `user_ctx` on every round trip.
            slopos_sched::task::init_user_ctx_for_new_task(
                child.user_ctx.get_mut(),
                entry,
                stack_ptr,
                0,
            );

            // POSIX_SPAWN_SETSIGDEF.
            if sigdefault_mask != 0 {
                task_default_signals_in_mask(child, sigdefault_mask);
            }

            let Some(inherited) = inherited else {
                return (None, None);
            };
            child.set_pgid(inherited.pgid);
            child.set_sid(inherited.sid);
            child.set_controlling_tty(inherited.ctty);
            // Exclusive rather than RCU: no reader can observe the child yet.
            // The displaced handle travels out because dropping a `KArc` under
            // the preempt guard could reach the buddy allocator's reuse path.
            let displaced = child.process_group.replace_exclusive(inherited.group);

            let fg = (flags & slopos_abi::task::TASK_FLAG_FOREGROUND != 0 && inherited.pgid != 0)
                .then_some(inherited.ctty)
                .flatten()
                .map(|ctty| (ctty, inherited.pgid, inherited.sid));
            (fg, displaced)
        }) else {
            return Err(ExecError::NoMem);
        };
        drop(displaced_group);

        // Findable from here but still not runnable: `publish_new_task` below
        // is the sole schedulable edge.
        let Some(registered) = spawn.commit() else {
            return Err(ExecError::NoMem);
        };

        // Only after `commit`: the edge must point at a live registry entry.
        if let Some(parent) = parent_ref.as_ref()
            && let Some(child_nn) = core::ptr::NonNull::new(registered.as_ptr())
        {
            link_child(parent, child_nn);
        }

        // Must precede the Ready-publish: a parent-side `tcsetpgrp` after spawn
        // returns leaves a window where the child is schedulable but still
        // background, and its first terminal read fails the foreground check.
        if let Some((ctty, child_pgid, child_sid)) = fg_handoff {
            use slopos_kernel_services::syscall_services::tty;
            // Checked variant: session match and set under one TTY lock, no
            // read-then-write TOCTOU.
            let _ = tty::set_foreground_pgrp_checked(ctty, child_pgid, child_sid);
        }

        // `publish_new_task`'s Release status store is what makes every write
        // above visible to the CPU that runs this task.
        if publish_new_task(&registered) != 0 {
            task_terminate(task_id);
            return Err(ExecError::NoMem);
        }

        Ok(task_id)
    })();

    result
}

pub fn do_exec(
    table: FdTable,
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

    let vm_process = table.process().ok_or(ExecError::NoMem)?;
    let exec_info = process_vm_load_elf_data(vm_process, elf_data.as_slice(), entry_out)
        .map_err(ExecError::from)?;

    if process_vm_reset_stack(vm_process) != 0 {
        return Err(ExecError::NoMem);
    }

    let stack_top = setup_user_stack(table, argv, envp, &exec_info)?;
    *stack_ptr_out = stack_top;
    *tls_tp_out = exec_info.tls_tp;

    // POSIX: close all FDs with FD_CLOEXEC set after point of no return.
    slopos_fs::fileio_close_on_exec(table);

    klog_info!(
        "exec: loaded ELF for process {}, entry={:#x}, stack={:#x}, tls_tp={:#x}",
        table.id(),
        *entry_out,
        stack_top,
        *tls_tp_out,
    );

    Ok(())
}

fn setup_user_stack(
    table: FdTable,
    argv: Option<&[&[u8]]>,
    envp: Option<&[&[u8]]>,
    exec_info: &ElfExecInfo,
) -> Result<u64, ExecError> {
    let vm_process = table.process().ok_or(ExecError::Fault)?;
    let stack_top_raw = process_vm_get_stack_top(vm_process);
    if stack_top_raw == 0 {
        return Err(ExecError::Fault);
    }
    let stack_top = stack_top_raw.wrapping_sub(8);

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
            write_to_user_stack(table, sp, arg)?;
            write_byte_to_user_stack(table, sp + arg.len() as u64, 0)?;
            string_ptrs.push(sp).map_err(|_| ExecError::NoMem)?;
        }
    }

    let argv_start = string_ptrs.len();

    if let Some(envs) = envp {
        for env in envs.iter() {
            let len = env.len() + 1;
            sp = sp.wrapping_sub(len as u64);
            sp &= !0x7;
            write_to_user_stack(table, sp, env)?;
            write_byte_to_user_stack(table, sp + env.len() as u64, 0)?;
            string_ptrs.push(sp).map_err(|_| ExecError::NoMem)?;
        }
    }

    sp &= !0xF;

    // SysV ABI: rsp must be 16-byte aligned at _start with argc at [rsp].
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
        write_u64_to_user_stack(table, slot, *a_type)?;
        write_u64_to_user_stack(table, slot + 8, *a_val)?;
    }

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(table, sp, 0)?;

    for i in (argv_start..string_ptrs.len()).rev() {
        sp = sp.wrapping_sub(8);
        write_u64_to_user_stack(table, sp, string_ptrs[i])?;
    }

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(table, sp, 0)?;

    for i in (0..argv_start).rev() {
        sp = sp.wrapping_sub(8);
        write_u64_to_user_stack(table, sp, string_ptrs[i])?;
    }

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(table, sp, argc as u64)?;

    Ok(sp)
}

fn write_to_user_stack(table: FdTable, addr: u64, data: &[u8]) -> Result<(), ExecError> {
    let vm_process = table.process().ok_or(ExecError::Fault)?;
    let vm_space = process_vm_get_vm_space(vm_process).ok_or(ExecError::Fault)?;
    process_vm_write_user_bytes(&vm_space, addr, data).map_err(|_| ExecError::Fault)
}

fn write_byte_to_user_stack(table: FdTable, addr: u64, byte: u8) -> Result<(), ExecError> {
    write_to_user_stack(table, addr, &[byte])
}

fn write_u64_to_user_stack(table: FdTable, addr: u64, value: u64) -> Result<(), ExecError> {
    let bytes = value.to_le_bytes();
    write_to_user_stack(table, addr, &bytes)
}
