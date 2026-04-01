//! exec() syscall implementation for loading and executing ELF binaries from filesystem.

#[cfg(feature = "itests")]
pub mod tests;

use alloc::vec::Vec;
use core::ffi::c_char;
use core::ptr;

use slopos_abi::addr::VirtAddr;
use slopos_abi::auxv::{AT_ENTRY, AT_NULL, AT_PAGESZ, AT_PHDR, AT_PHENT, AT_PHNUM};
use slopos_abi::task::{
    INVALID_PROCESS_ID, TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE, TASK_NAME_MAX_LEN,
};
use slopos_fs::fileio::{fileio_clone_table_for_process, fileio_destroy_table_for_process};
use slopos_fs::vfs::ops::vfs_open;
use slopos_mm::elf::{ElfError, ElfExecInfo};
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_mm::process_vm::{
    process_vm_get_page_dir, process_vm_get_stack_top, process_vm_load_elf_data,
    process_vm_reset_stack,
};
use slopos_utils::klog_info;

use crate::sched::schedule_new_task;
use crate::scheduler::task_struct::Task;
use crate::task::{TaskEntry, task_create, task_find_by_id, task_get_info, task_terminate};
use slopos_abi::task::INVALID_TASK_ID;

extern crate alloc;

pub const EXEC_MAX_PATH: usize = 256;
pub const EXEC_MAX_ARG_STRLEN: usize = 4096;
pub const EXEC_MAX_ARGS: usize = 32;
pub const EXEC_MAX_ENVS: usize = 32;
pub const EXEC_MAX_ELF_SIZE: usize = 16 * 1024 * 1024;
pub const EXEC_SPAWN_DEFAULT_PRIORITY: u8 = 5;

pub const INIT_PATH: &[u8] = b"/sbin/init";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExecError {
    NoEntry = -2,
    NoExec = -8,
    NoMem = -12,
    Fault = -14,
    NameTooLong = -36,
    IoError = -5,
    TooManyArgs = -7,
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
        EXEC_SPAWN_DEFAULT_PRIORITY,
        TASK_FLAG_USER_MODE | TASK_FLAG_SYSTEM,
        INVALID_PROCESS_ID,
        INVALID_TASK_ID,
    )
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
    priority: u8,
    mut flags: u16,
    inherit_fds_from: u32,
    parent_task_id: u32,
) -> Result<u32, ExecError> {
    let result = (|| {
        let normalized_path = trim_nul_bytes(path);
        if normalized_path.is_empty() || normalized_path.len() > EXEC_MAX_PATH {
            return Err(ExecError::NameTooLong);
        }

        flags |= TASK_FLAG_USER_MODE;
        let task_name = task_name_from_path(normalized_path)?;
        let user_code_entry: TaskEntry =
            unsafe { core::mem::transmute(PROCESS_CODE_START_VA as usize) };

        let task_id = task_create(
            task_name.as_ptr() as *const c_char,
            user_code_entry,
            ptr::null_mut(),
            priority,
            flags,
        );

        if task_id == INVALID_TASK_ID {
            return Err(ExecError::NoMem);
        }

        let mut task_info: *mut Task = ptr::null_mut();
        if task_get_info(task_id, &mut task_info) != 0 || task_info.is_null() {
            task_terminate(task_id);
            return Err(ExecError::Fault);
        }

        // Reset to Blocked so the scheduler cannot pick this task up
        // while we continue writing fields (entry_point, rip, rsp, fd
        // table, pgid, etc.).  We will re-publish as Ready right before
        // schedule_new_task.  This is the Linux TASK_NEW pattern:
        // task_create sets Ready for simple callers, but we need a
        // longer initialization window that must not be visible to other
        // CPUs.
        unsafe {
            (*task_info).set_status(slopos_abi::task::TaskStatus::Blocked);
        }

        let process_id = unsafe { (*task_info).process_id };
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
            task_terminate(task_id);
            return Err(err);
        }

        unsafe {
            (*task_info).entry_point = entry;
            ptr::write_unaligned(ptr::addr_of_mut!((*task_info).context.rip), entry);
            ptr::write_unaligned(ptr::addr_of_mut!((*task_info).context.rsp), stack_ptr);
            (*task_info).fs_base = tls_tp;

            // Update the synthetic InterruptFrame on the kernel stack.
            // init_task_context pushed it at (kernel_stack_top - sizeof(InterruptFrame) - 8)
            // (the -8 is the ret_from_fork return address slot).
            let frame_size = core::mem::size_of::<slopos_arch::InterruptFrame>() as u64;
            let frame_addr = (*task_info).kernel_stack_top - frame_size;
            let frame_ptr = frame_addr as *mut slopos_arch::InterruptFrame;
            ptr::write_unaligned(ptr::addr_of_mut!((*frame_ptr).rip), entry);
            ptr::write_unaligned(ptr::addr_of_mut!((*frame_ptr).rsp), stack_ptr);
        }

        // Clone the parent's fd table BEFORE scheduling so the child has
        // stdin/stdout/stderr available from the moment it starts running.
        // This avoids an SMP race where the child runs before the parent
        // can set up the fd table post-spawn.
        if inherit_fds_from != INVALID_PROCESS_ID {
            fileio_destroy_table_for_process(process_id);
            let _ = fileio_clone_table_for_process(inherit_fds_from, process_id);
        }

        // Inherit job-control state (pgid, sid, controlling_tty) from the
        // parent task so spawned children participate in the parent's session.
        // This matches fork semantics and is required for proper job control:
        // without it, the child is in its own session and the shell cannot
        // set it as the foreground process group.
        if parent_task_id != INVALID_TASK_ID {
            let parent_ptr = task_find_by_id(parent_task_id);
            if !parent_ptr.is_null() {
                let parent = unsafe { &*parent_ptr };
                let child = unsafe { &mut *task_info };
                if flags & slopos_abi::task::TASK_FLAG_NEW_PGRP != 0 {
                    child.pgid = task_id;
                } else {
                    child.pgid = parent.pgid;
                }
                child.sid = parent.sid;
                child.controlling_tty = parent.controlling_tty;
                child.parent_task_id = parent_task_id;
            }
        }

        // Publish the task as Ready only after ALL field writes are done.
        // The Release ordering on this atomic store ensures that every
        // prior write (process_id, entry_point, rip, rsp, fd table,
        // pgid, sid, controlling_tty) is visible to the CPU that
        // eventually runs this task.  This is the Linux TASK_NEW →
        // TASK_RUNNING pattern: task_create leaves the task Blocked,
        // and we make it schedulable only here.
        unsafe { (*task_info).set_status(slopos_abi::task::TaskStatus::Ready) };

        if schedule_new_task(task_info) != 0 {
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

    let mut elf_data: Vec<u8> = Vec::new();
    elf_data
        .try_reserve(file_size)
        .map_err(|_| ExecError::NoMem)?;
    elf_data.resize(file_size, 0);

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

    let argc = argv.map(|a| a.len()).unwrap_or(0);
    let envc = envp.map(|e| e.len()).unwrap_or(0);

    if argc > EXEC_MAX_ARGS || envc > EXEC_MAX_ENVS {
        return Err(ExecError::TooManyArgs);
    }

    let mut sp = stack_top;
    sp = sp.wrapping_sub(128);
    sp &= !0xF;

    let mut string_ptrs: Vec<u64> = Vec::new();
    string_ptrs
        .try_reserve(argc + envc + 2)
        .map_err(|_| ExecError::NoMem)?;

    if let Some(args) = argv {
        for arg in args.iter() {
            let len = arg.len() + 1;
            sp = sp.wrapping_sub(len as u64);
            sp &= !0x7;
            write_to_user_stack(page_dir, sp, arg)?;
            write_byte_to_user_stack(page_dir, sp + arg.len() as u64, 0)?;
            string_ptrs.push(sp);
        }
    }

    let argv_start = string_ptrs.len();

    if let Some(envs) = envp {
        for env in envs.iter() {
            let len = env.len() + 1;
            sp = sp.wrapping_sub(len as u64);
            sp &= !0x7;
            write_to_user_stack(page_dir, sp, env)?;
            write_byte_to_user_stack(page_dir, sp + env.len() as u64, 0)?;
            string_ptrs.push(sp);
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
        write_u64_to_user_stack(page_dir, slot, *a_type)?;
        write_u64_to_user_stack(page_dir, slot + 8, *a_val)?;
    }

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(page_dir, sp, 0)?;

    for i in (argv_start..string_ptrs.len()).rev() {
        sp = sp.wrapping_sub(8);
        write_u64_to_user_stack(page_dir, sp, string_ptrs[i])?;
    }

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(page_dir, sp, 0)?;

    for i in (0..argv_start).rev() {
        sp = sp.wrapping_sub(8);
        write_u64_to_user_stack(page_dir, sp, string_ptrs[i])?;
    }

    sp = sp.wrapping_sub(8);
    write_u64_to_user_stack(page_dir, sp, argc as u64)?;

    Ok(sp)
}

fn write_to_user_stack(
    page_dir: *mut slopos_mm::paging::ProcessPageDir,
    addr: u64,
    data: &[u8],
) -> Result<(), ExecError> {
    use slopos_mm::paging::virt_to_phys_in_dir;

    for (i, &byte) in data.iter().enumerate() {
        let va = addr + i as u64;
        let page_va = va & !(PAGE_SIZE_4KB - 1);
        let page_off = (va & (PAGE_SIZE_4KB - 1)) as usize;

        let phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(page_va));
        if phys.is_null() {
            return Err(ExecError::Fault);
        }
        let virt = phys.to_virt();
        if virt.is_null() {
            return Err(ExecError::Fault);
        }
        unsafe {
            *virt.as_mut_ptr::<u8>().add(page_off) = byte;
        }
    }
    Ok(())
}

fn write_byte_to_user_stack(
    page_dir: *mut slopos_mm::paging::ProcessPageDir,
    addr: u64,
    byte: u8,
) -> Result<(), ExecError> {
    write_to_user_stack(page_dir, addr, &[byte])
}

fn write_u64_to_user_stack(
    page_dir: *mut slopos_mm::paging::ProcessPageDir,
    addr: u64,
    value: u64,
) -> Result<(), ExecError> {
    let bytes = value.to_le_bytes();
    write_to_user_stack(page_dir, addr, &bytes)
}
