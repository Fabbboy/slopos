//! exec() syscall implementation for loading and executing ELF binaries from filesystem.

use alloc::vec::Vec;

use slopos_abi::addr::VirtAddr;
use slopos_fs::vfs::ops::vfs_open;
use slopos_lib::klog_info;
use slopos_mm::elf::{ElfError, ElfValidator};
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::mm_constants::{PAGE_SIZE_4KB, PROCESS_CODE_START_VA};
use slopos_mm::process_vm::process_vm_get_page_dir;

extern crate alloc;

pub const EXEC_MAX_PATH: usize = 256;
pub const EXEC_MAX_ARG_STRLEN: usize = 4096;
pub const EXEC_MAX_ARGS: usize = 32;
pub const EXEC_MAX_ENVS: usize = 32;
pub const EXEC_MAX_ELF_SIZE: usize = 16 * 1024 * 1024;

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

pub fn do_exec(
    process_id: u32,
    path: &[u8],
    argv: Option<&[&[u8]]>,
    envp: Option<&[&[u8]]>,
    entry_out: &mut u64,
    stack_ptr_out: &mut u64,
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

    let file_size = handle.size().map_err(|_| ExecError::IoError)? as usize;
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

    let validator = ElfValidator::new(&elf_data)
        .map_err(|_| ExecError::NoExec)?
        .with_load_base(PROCESS_CODE_START_VA);

    let header = validator.header();
    let (segments, segment_count) = validator
        .validate_load_segments()
        .map_err(|_| ExecError::NoExec)?;
    let segments = &segments[..segment_count];

    if segments.is_empty() {
        return Err(ExecError::NoExec);
    }

    let page_dir = process_vm_get_page_dir(process_id);
    if page_dir.is_null() {
        return Err(ExecError::NoMem);
    }

    let min_vaddr = segments.iter().map(|s| s.original_vaddr).min().unwrap_or(0);
    let _needs_reloc = min_vaddr >= 0xFFFF_FFFF_8000_0000 || min_vaddr != PROCESS_CODE_START_VA;

    clear_user_code_region(page_dir, PROCESS_CODE_START_VA);

    for segment in segments.iter() {
        let user_start =
            translate_address(segment.original_vaddr, min_vaddr, PROCESS_CODE_START_VA);
        let user_end = translate_address(
            segment.original_vaddr + segment.mem_size,
            min_vaddr,
            PROCESS_CODE_START_VA,
        );

        map_segment(page_dir, &elf_data, segment, user_start, user_end)?;
    }

    let user_entry = translate_address(header.e_entry, min_vaddr, PROCESS_CODE_START_VA);
    *entry_out = user_entry;

    let stack_top = setup_user_stack(process_id, argv, envp)?;
    *stack_ptr_out = stack_top;

    klog_info!(
        "exec: loaded ELF for process {}, entry={:#x}, stack={:#x}",
        process_id,
        user_entry,
        stack_top
    );

    Ok(())
}

fn translate_address(addr: u64, min_vaddr: u64, code_base: u64) -> u64 {
    const KERNEL_BASE: u64 = 0xFFFF_FFFF_8000_0000;
    if addr >= KERNEL_BASE {
        let offset = addr.wrapping_sub(KERNEL_BASE);
        code_base.wrapping_add(offset)
    } else if min_vaddr >= KERNEL_BASE {
        let offset = addr.wrapping_sub(min_vaddr);
        code_base.wrapping_add(offset)
    } else if min_vaddr < code_base {
        addr.wrapping_add(code_base.wrapping_sub(min_vaddr))
    } else {
        addr
    }
}

fn clear_user_code_region(page_dir: *mut slopos_mm::paging::ProcessPageDir, code_base: u64) {
    let code_limit = code_base + 0x100000;
    slopos_mm::process_vm::unmap_user_range_pub(
        page_dir,
        code_base.saturating_sub(0x100000),
        code_limit + 0x100000,
    );
}

fn map_segment(
    page_dir: *mut slopos_mm::paging::ProcessPageDir,
    elf_data: &[u8],
    segment: &slopos_mm::elf::ValidatedSegment,
    user_start: u64,
    user_end: u64,
) -> Result<(), ExecError> {
    use slopos_lib::align_down;
    use slopos_mm::elf::PF_W;
    use slopos_mm::mm_constants::PageFlags;
    use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame};
    use slopos_mm::paging::{map_page_4kb_in_dir, virt_to_phys_in_dir};

    let map_flags = if (segment.flags & PF_W) != 0 {
        PageFlags::USER_RW.bits()
    } else {
        PageFlags::USER_RO.bits()
    };

    let page_start = align_down(user_start as usize, PAGE_SIZE_4KB as usize) as u64;
    let page_end = slopos_lib::align_up(user_end as usize, PAGE_SIZE_4KB as usize) as u64;

    let mut dst = page_start;
    while dst < page_end {
        let existing_phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(dst));
        let phys = if !existing_phys.is_null() {
            existing_phys
        } else {
            let new_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
            if new_phys.is_null() {
                return Err(ExecError::NoMem);
            }
            if map_page_4kb_in_dir(page_dir, VirtAddr::new(dst), new_phys, map_flags) != 0 {
                free_page_frame(new_phys);
                return Err(ExecError::NoMem);
            }
            new_phys
        };

        let dest_virt = phys.to_virt();
        if dest_virt.is_null() {
            return Err(ExecError::NoMem);
        }

        copy_segment_page_data(elf_data, segment, dst, user_start, dest_virt.as_mut_ptr());

        dst += PAGE_SIZE_4KB;
    }

    Ok(())
}

fn copy_segment_page_data(
    data: &[u8],
    segment: &slopos_mm::elf::ValidatedSegment,
    page_va: u64,
    user_seg_start: u64,
    dest_ptr: *mut u8,
) {
    let page_end_va = page_va.wrapping_add(PAGE_SIZE_4KB);
    let seg_file_end = user_seg_start.wrapping_add(segment.file_size);
    let seg_mem_end = user_seg_start.wrapping_add(segment.mem_size);

    let copy_start = core::cmp::max(page_va, user_seg_start);
    let copy_end = core::cmp::min(page_end_va, seg_file_end);

    if copy_start < copy_end {
        let page_off_in_seg = copy_start - user_seg_start;
        let dest_off = (copy_start - page_va) as usize;
        let copy_len = (copy_end - copy_start) as usize;
        let src_off = segment.file_offset.wrapping_add(page_off_in_seg) as usize;

        if src_off < data.len() && src_off.saturating_add(copy_len) <= data.len() {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    data.as_ptr().add(src_off),
                    dest_ptr.add(dest_off),
                    copy_len,
                );
            }
        }
    }

    if seg_mem_end > seg_file_end {
        let zero_start = core::cmp::max(page_va, seg_file_end);
        let zero_end = core::cmp::min(page_end_va, seg_mem_end);
        if zero_start < zero_end {
            let zero_off = (zero_start - page_va) as usize;
            let zero_len = (zero_end - zero_start) as usize;
            unsafe {
                core::ptr::write_bytes(dest_ptr.add(zero_off), 0, zero_len);
            }
        }
    }
}

fn setup_user_stack(
    process_id: u32,
    argv: Option<&[&[u8]]>,
    envp: Option<&[&[u8]]>,
) -> Result<u64, ExecError> {
    let layout = unsafe { &*slopos_mm::memory_layout::mm_get_process_layout() };
    let stack_top = layout.stack_top;

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

    let aux_size = 2 * 8;
    sp = sp.wrapping_sub(aux_size);
    write_u64_to_user_stack(page_dir, sp, 0)?;
    write_u64_to_user_stack(page_dir, sp + 8, 0)?;

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

    sp &= !0xF;
    if ((stack_top - sp) / 8) % 2 != 0 {
        sp = sp.wrapping_sub(8);
    }

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
