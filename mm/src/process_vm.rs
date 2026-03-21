use core::ffi::c_int;
use core::ptr;

use slopos_abi::addr::VirtAddr;
use slopos_sync::IrqMutex;
use slopos_utils::{align_down, align_up, klog_debug, klog_info};

use crate::aslr;
use crate::elf::{ElfError, ElfValidator, MAX_LOAD_SEGMENTS, PF_W, ValidatedSegment};
use crate::hhdm::PhysAddrHhdm;
use crate::kernel_heap::{kfree, kmalloc};
use crate::memory_layout_defs::DEFAULT_PROCESS_LAYOUT;
use crate::memory_layout_defs::{KERNEL_VIRTUAL_BASE, MAX_PROCESSES, PROCESS_TLS_BASE_VA};
use crate::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame, page_frame_inc_ref};
use crate::paging::{
    PageTable, ProcessPageDir, map_page_4kb_in_dir, paging_copy_kernel_mappings,
    paging_free_user_space, paging_get_pte_flags, paging_mark_cow, paging_mark_range_user,
    paging_sync_kernel_mappings, unmap_page_in_dir, virt_to_phys_in_dir,
};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::tlb;
use crate::vma_flags::VmaFlags;
use crate::vma_tree::{VmaNode, VmaTree};
use slopos_abi::task::INVALID_PROCESS_ID;

#[derive(Clone, Copy)]
struct ProcessVm {
    process_id: u32,
    page_dir: *mut ProcessPageDir,
    vma_tree: VmaTree,
    code_start: u64,
    data_start: u64,
    heap_start: u64,
    heap_end: u64,
    stack_start: u64,
    stack_end: u64,
    total_pages: u32,
    flags: u32,
    next: *mut ProcessVm,
}

unsafe impl Send for ProcessVm {}

impl ProcessVm {
    const fn new() -> Self {
        Self {
            process_id: INVALID_PROCESS_ID,
            page_dir: ptr::null_mut(),
            vma_tree: VmaTree::new(),
            code_start: 0,
            data_start: 0,
            heap_start: 0,
            heap_end: 0,
            stack_start: 0,
            stack_end: 0,
            total_pages: 0,
            flags: 0,
            next: ptr::null_mut(),
        }
    }

    fn reset(&mut self) {
        self.process_id = INVALID_PROCESS_ID;
        self.page_dir = ptr::null_mut();
        self.code_start = 0;
        self.data_start = 0;
        self.heap_start = 0;
        self.heap_end = 0;
        self.stack_start = 0;
        self.stack_end = 0;
        self.total_pages = 0;
        self.flags = 0;
        self.next = ptr::null_mut();
    }
}

struct VmManager {
    processes: [ProcessVm; MAX_PROCESSES],
    num_processes: u32,
    next_process_id: u32,
    active_process: *mut ProcessVm,
    process_list: *mut ProcessVm,
}

unsafe impl Send for VmManager {}

impl VmManager {
    const fn new() -> Self {
        Self {
            processes: [ProcessVm::new(); MAX_PROCESSES],
            num_processes: 0,
            next_process_id: 1,
            active_process: ptr::null_mut(),
            process_list: ptr::null_mut(),
        }
    }
}

static VM_MANAGER: IrqMutex<VmManager> = IrqMutex::new(VmManager::new());

fn vma_range_valid(start: u64, end: u64) -> bool {
    start < end && (start & (PAGE_SIZE_4KB - 1)) == 0 && (end & (PAGE_SIZE_4KB - 1)) == 0
}

fn map_user_range(
    page_dir: *mut ProcessPageDir,
    start_addr: u64,
    end_addr: u64,
    map_flags: u64,
    pages_mapped_out: *mut u32,
) -> c_int {
    if page_dir.is_null() {
        klog_info!("map_user_range: Missing page directory");
        return -1;
    }
    if (start_addr & (PAGE_SIZE_4KB - 1)) != 0
        || (end_addr & (PAGE_SIZE_4KB - 1)) != 0
        || end_addr <= start_addr
    {
        klog_info!("map_user_range: Unaligned or invalid range");
        return -1;
    }

    let mut current = start_addr;
    let mut mapped: u32 = 0;

    while current < end_addr {
        let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
        if phys.is_null() {
            klog_info!("map_user_range: Physical allocation failed");
            rollback_range(page_dir, current, start_addr, &mut mapped);
            if !pages_mapped_out.is_null() {
                unsafe { *pages_mapped_out = 0 };
            }
            return -1;
        }
        if map_page_4kb_in_dir(page_dir, VirtAddr::new(current), phys, map_flags) != 0 {
            klog_info!("map_user_range: Virtual mapping failed");
            free_page_frame(phys);
            rollback_range(page_dir, current, start_addr, &mut mapped);
            if !pages_mapped_out.is_null() {
                unsafe { *pages_mapped_out = 0 };
            }
            return -1;
        }
        mapped += 1;
        current += PAGE_SIZE_4KB;
    }

    if !pages_mapped_out.is_null() {
        unsafe { *pages_mapped_out = mapped };
    }
    0
}

fn rollback_range(
    page_dir: *mut ProcessPageDir,
    mut current: u64,
    start_addr: u64,
    mapped: &mut u32,
) {
    while *mapped > 0 {
        current -= PAGE_SIZE_4KB;
        let phys = unmap_page_in_dir(page_dir, VirtAddr::new(current));
        if !phys.is_null() {
            free_page_frame(phys);
        }
        *mapped -= 1;
    }
    let _ = start_addr;
}

fn unmap_user_range(page_dir: *mut ProcessPageDir, start_addr: u64, end_addr: u64) {
    if end_addr <= start_addr || page_dir.is_null() {
        return;
    }
    let mut addr = start_addr;
    while addr < end_addr {
        let phys = unmap_page_in_dir(page_dir, VirtAddr::new(addr));
        if !phys.is_null() {
            free_page_frame(phys);
        }
        addr += PAGE_SIZE_4KB;
    }
}

fn find_process_vm(process_id: u32) -> *mut ProcessVm {
    let manager = VM_MANAGER.lock();
    for process in manager.processes.iter() {
        if process.process_id == process_id {
            return process as *const _ as *mut ProcessVm;
        }
    }
    ptr::null_mut()
}
pub fn process_vm_get_page_dir(process_id: u32) -> *mut ProcessPageDir {
    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return ptr::null_mut();
    }
    unsafe { (*process_ptr).page_dir }
}

pub fn process_vm_sync_kernel_mappings(process_id: u32) {
    let page_dir = process_vm_get_page_dir(process_id);
    if page_dir.is_null() {
        return;
    }
    paging_sync_kernel_mappings(page_dir);
}

fn add_vma_to_process(process: *mut ProcessVm, start: u64, end: u64, flags: VmaFlags) -> c_int {
    if process.is_null() || !vma_range_valid(start, end) {
        return -1;
    }
    unsafe {
        let tree = &mut (*process).vma_tree;

        let overlap = tree.find_overlapping(start, end);
        if !overlap.is_null() && (*overlap).flags != flags {
            klog_info!("add_vma_to_process: Overlap with incompatible VMA");
            return -1;
        }

        let node = tree.insert(start, end, flags);
        if node.is_null() {
            klog_info!("add_vma_to_process: Failed to allocate VMA");
            return -1;
        }

        try_merge_adjacent(tree, node);
    }
    0
}

unsafe fn try_merge_adjacent(tree: &mut VmaTree, node: *mut VmaNode) {
    if node.is_null() {
        return;
    }

    let start = (*node).start;
    let end = (*node).end;
    let flags = (*node).flags;

    let prev = tree.find_overlapping(start.saturating_sub(1), start);
    if !prev.is_null() && prev != node && (*prev).end == start && (*prev).flags == flags {
        let new_start = (*prev).start;
        tree.remove((*prev).start, (*prev).end);
        tree.set_start(node, new_start);
    }

    let next = tree.find_overlapping(end, end.saturating_add(1));
    if !next.is_null() && next != node && (*next).start == (*node).end && (*next).flags == flags {
        let new_end = (*next).end;
        tree.remove((*next).start, (*next).end);
        tree.set_end(node, new_end);
    }
}

fn remove_vma_from_process(process: *mut ProcessVm, start: u64, end: u64) -> c_int {
    if process.is_null() || !vma_range_valid(start, end) {
        return -1;
    }
    unsafe {
        if (*process).vma_tree.remove(start, end) {
            0
        } else {
            -1
        }
    }
}

fn find_vma_covering(process: *mut ProcessVm, start: u64, end: u64) -> *mut VmaNode {
    if process.is_null() || !vma_range_valid(start, end) {
        return ptr::null_mut();
    }
    unsafe { (*process).vma_tree.find_covering(start, end) }
}

fn unmap_and_free_range(process: *mut ProcessVm, start: u64, end: u64) -> u32 {
    if process.is_null() || unsafe { (*process).page_dir.is_null() } || !vma_range_valid(start, end)
    {
        return 0;
    }
    let mut freed = 0u32;
    let mut addr = start;
    unsafe {
        while addr < end {
            let phys = unmap_page_in_dir((*process).page_dir, VirtAddr::new(addr));
            if !phys.is_null() {
                free_page_frame(phys);
                freed += 1;
            }
            addr += PAGE_SIZE_4KB;
        }
    }
    freed
}

fn teardown_process_mappings(process: *mut ProcessVm) {
    if process.is_null() || unsafe { (*process).page_dir.is_null() } {
        return;
    }
    unsafe {
        let tree = &mut (*process).vma_tree;
        let mut cursor = tree.first();
        while !cursor.is_null() {
            let next = tree.next(cursor);
            let freed = unmap_and_free_range(process, (*cursor).start, (*cursor).end);
            if (*process).total_pages >= freed {
                (*process).total_pages -= freed;
            } else {
                (*process).total_pages = 0;
            }
            cursor = next;
        }
        tree.clear();
        (*process).heap_end = (*process).heap_start;
    }
}

// ELF structures for relocation parsing
#[repr(C)]
struct Elf64Shdr {
    sh_name: u32,
    sh_type: u32,
    sh_flags: u64,
    sh_addr: u64,
    sh_offset: u64,
    sh_size: u64,
    sh_link: u32,
    sh_info: u32,
    sh_addralign: u64,
    sh_entsize: u64,
}

#[repr(C)]
struct Elf64Rela {
    r_offset: u64,
    r_info: u64,
    r_addend: i64,
}

// ELF section types
const SHT_RELA: u32 = 4;

// x86-64 relocation types
const R_X86_64_64: u32 = 1; // Absolute 64-bit
const R_X86_64_PC32: u32 = 2; // RIP-relative 32-bit
const R_X86_64_32: u32 = 10; // Absolute 32-bit
const R_X86_64_32S: u32 = 11; // Absolute 32-bit sign-extended

fn apply_elf_relocations(
    payload: *const u8,
    payload_len: usize,
    page_dir: *mut ProcessPageDir,
    section_mappings: &[(u64, u64, u64)], // (kernel_va_start, kernel_va_end, user_va_start)
) -> c_int {
    if payload.is_null() || page_dir.is_null() {
        return -1;
    }

    #[repr(C)]
    struct Elf64Ehdr {
        ident: [u8; 16],
        e_type: u16,
        e_machine: u16,
        e_version: u32,
        e_entry: u64,
        e_phoff: u64,
        e_shoff: u64,
        e_flags: u32,
        e_ehsize: u16,
        e_phentsize: u16,
        e_phnum: u16,
        e_shentsize: u16,
        e_shnum: u16,
        e_shstrndx: u16,
    }

    let ehdr = unsafe { &*(payload as *const Elf64Ehdr) };
    if &ehdr.ident[0..4] != b"\x7fELF" || ehdr.e_shoff == 0 || ehdr.e_shnum == 0 {
        return -1;
    }

    let sh_size = ehdr.e_shentsize as usize;
    let sh_num = ehdr.e_shnum as usize;
    let sh_off = ehdr.e_shoff as usize;
    let shstrndx = ehdr.e_shstrndx as usize;

    if sh_off + sh_num * sh_size > payload_len || shstrndx >= sh_num {
        return -1;
    }

    // Get string table for section names
    let shstrtab_shdr = unsafe { &*(payload.add(sh_off + shstrndx * sh_size) as *const Elf64Shdr) };
    let shstrtab_base = shstrtab_shdr.sh_offset as usize;
    let shstrtab_size = shstrtab_shdr.sh_size as usize;
    if shstrtab_base + shstrtab_size > payload_len {
        return -1;
    }

    // Helper to get section name
    let get_section_name = |sh_name_off: u32| -> Option<&[u8]> {
        let off = shstrtab_base + sh_name_off as usize;
        if off >= payload_len {
            return None;
        }
        let start = unsafe { payload.add(off) };
        let mut len = 0;
        while off + len < payload_len && unsafe { *start.add(len) } != 0 {
            len += 1;
        }
        Some(unsafe { core::slice::from_raw_parts(start, len) })
    };

    // Helper to map kernel VA to user VA
    let map_kernel_va_to_user = |kernel_va: u64| -> Option<u64> {
        for &(kern_start, kern_end, user_start) in section_mappings {
            if kernel_va >= kern_start && kernel_va < kern_end {
                return Some(user_start + (kernel_va - kern_start));
            }
        }
        None
    };

    // Iterate through section headers to find .rela sections
    for i in 0..sh_num {
        let shdr = unsafe { &*(payload.add(sh_off + i * sh_size) as *const Elf64Shdr) };
        if shdr.sh_type != SHT_RELA {
            continue;
        }

        let name_off = shdr.sh_name;
        let Some(name) = get_section_name(name_off) else {
            continue;
        };

        // Check if this is a .rela section we care about
        if !name.starts_with(b".rela.") {
            continue;
        }

        // Find the target section this relocation applies to
        let target_section_idx = shdr.sh_info as usize;
        if target_section_idx >= sh_num {
            continue;
        }
        let target_shdr =
            unsafe { &*(payload.add(sh_off + target_section_idx * sh_size) as *const Elf64Shdr) };

        // Get the target section's user VA mapping
        let target_kern_va = target_shdr.sh_addr;
        let Some(target_user_va_base) = map_kernel_va_to_user(target_kern_va) else {
            continue;
        };

        // Process relocation entries
        let rela_base = shdr.sh_offset as usize;
        let rela_size = shdr.sh_size as usize;
        let rela_entsize = if shdr.sh_entsize != 0 {
            shdr.sh_entsize as usize
        } else {
            core::mem::size_of::<Elf64Rela>()
        };

        if rela_base + rela_size > payload_len {
            continue;
        }

        let num_relocs = rela_size / rela_entsize;
        for j in 0..num_relocs {
            let rela_ptr = unsafe { payload.add(rela_base + j * rela_entsize) as *const Elf64Rela };
            let rela = unsafe { &*rela_ptr };

            let reloc_type = (rela.r_info & 0xffffffff) as u32;
            let _symbol_idx = (rela.r_info >> 32) as u32;

            // Calculate relocation address in user space
            // r_offset is an absolute address in the ELF's VAs (kernel VAs)
            // We need to convert it to user space: user_addr = user_base + (kern_addr - kern_base)
            let reloc_kern_addr = rela.r_offset; // r_offset is already absolute in kernel VAs
            let reloc_user_addr = if reloc_kern_addr >= target_kern_va {
                target_user_va_base + (reloc_kern_addr - target_kern_va)
            } else {
                // r_offset might be relative, try adding to target_user_va_base
                target_user_va_base.wrapping_add(rela.r_offset)
            };

            // Calculate symbol VA based on relocation type
            // For R_X86_64_PLT32/PC32: read current offset, calculate symbol = rip_after + offset + addend
            // For others: use addend or read from target
            let symbol_va = match reloc_type {
                R_X86_64_PC32 | 4 => {
                    // 4 = R_X86_64_PLT32
                    // For PC32/PLT32, read current offset from instruction and calculate symbol
                    let read_page_va = reloc_user_addr & !(PAGE_SIZE_4KB - 1);
                    let read_page_off = (reloc_user_addr & (PAGE_SIZE_4KB - 1)) as usize;
                    let read_phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(read_page_va));
                    if read_phys.is_null() {
                        continue;
                    }
                    let read_virt = read_phys.to_virt();
                    if read_virt.is_null() {
                        continue;
                    }
                    let read_ptr = unsafe { read_virt.as_mut_ptr::<u8>().add(read_page_off) };
                    let current_offset =
                        unsafe { core::ptr::read_unaligned(read_ptr as *const i32) } as i64;
                    // For R_X86_64_PC32/PLT32: offset = S + A - P, where:
                    //   S = symbol value, A = addend, P = place (RIP after instruction)
                    // The current_offset in the instruction was calculated for kernel addresses.
                    // We need to find the original symbol address, then map it to user space.
                    // Original: current_offset = original_symbol + addend - original_kernel_rip_after
                    // So: original_symbol = current_offset - addend + original_kernel_rip_after
                    let original_kernel_rip_after = reloc_kern_addr.wrapping_add(4);
                    // For PC32: offset = S + A - P, so S = offset - A + P = offset + P - A
                    // But we need to be careful: if A is negative, subtracting it means adding
                    let original_symbol_va = (original_kernel_rip_after as i64)
                        .wrapping_add(current_offset)
                        .wrapping_sub(rela.r_addend)
                        as u64;
                    original_symbol_va
                }
                _ => {
                    if rela.r_addend != 0 {
                        rela.r_addend as u64
                    } else {
                        // If addend is 0, try reading current value
                        let read_page_va = reloc_user_addr & !(PAGE_SIZE_4KB - 1);
                        let read_page_off = (reloc_user_addr & (PAGE_SIZE_4KB - 1)) as usize;
                        let read_phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(read_page_va));
                        if read_phys.is_null() {
                            continue;
                        }
                        let read_virt = read_phys.to_virt();
                        if read_virt.is_null() {
                            continue;
                        }
                        let read_ptr = unsafe { read_virt.as_mut_ptr::<u8>().add(read_page_off) };
                        match reloc_type {
                            R_X86_64_64 => unsafe {
                                core::ptr::read_unaligned(read_ptr as *const u64)
                            },
                            R_X86_64_32 | R_X86_64_32S => {
                                let val =
                                    unsafe { core::ptr::read_unaligned(read_ptr as *const u32) }
                                        as u64;
                                if reloc_type == R_X86_64_32S {
                                    (val as i32 as i64) as u64
                                } else {
                                    val
                                }
                            }
                            _ => continue,
                        }
                    }
                }
            };

            // Map symbol VA to user VA
            let Some(user_symbol_va) = map_kernel_va_to_user(symbol_va) else {
                // Symbol might be in a section we haven't mapped, skip
                continue;
            };

            // Get physical page for this address
            let reloc_page_va = reloc_user_addr & !(PAGE_SIZE_4KB - 1);
            let reloc_page_off = (reloc_user_addr & (PAGE_SIZE_4KB - 1)) as usize;

            let reloc_phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(reloc_page_va));
            if reloc_phys.is_null() {
                continue;
            }
            let reloc_virt = reloc_phys.to_virt();
            if reloc_virt.is_null() {
                continue;
            }

            let reloc_ptr = unsafe { reloc_virt.as_mut_ptr::<u8>().add(reloc_page_off) };

            // Apply relocation based on type
            match reloc_type {
                R_X86_64_64 => {
                    // Absolute 64-bit: write symbol value directly
                    unsafe {
                        core::ptr::write_unaligned(reloc_ptr as *mut u64, user_symbol_va);
                    }
                }
                R_X86_64_PC32 | 4 => {
                    // 4 = R_X86_64_PLT32, same as PC32 for static binaries
                    // RIP-relative 32-bit: offset = symbol - (RIP after instruction)
                    let rip_after = reloc_user_addr + 4; // 32-bit = 4 bytes
                    let offset = (user_symbol_va as i64 - rip_after as i64) as i32;
                    unsafe {
                        core::ptr::write_unaligned(reloc_ptr as *mut i32, offset);
                    }
                }
                R_X86_64_32 | R_X86_64_32S => {
                    // Absolute 32-bit: write lower 32 bits of symbol value
                    unsafe {
                        core::ptr::write_unaligned(reloc_ptr as *mut u32, user_symbol_va as u32);
                    }
                }
                _ => {
                    // Unknown relocation type, skip
                    continue;
                }
            }
        }
    }

    0
}

pub fn process_vm_load_elf_data(
    process_id: u32,
    data: &[u8],
    entry_out: &mut u64,
) -> Result<crate::elf::ElfExecInfo, ElfError> {
    let code_base = crate::memory_layout_defs::PROCESS_CODE_START_VA;

    let validator = ElfValidator::new(data)?.with_load_base(code_base);
    let header = validator.header();

    // Reject dynamically-linked binaries (PT_INTERP present).
    if validator.has_interpreter()? {
        return Err(ElfError::DynamicNotSupported);
    }

    let (segments, segment_count) = validator.validate_load_segments()?;
    let segments = &segments[..segment_count];

    let tls_segment = validator.find_tls_segment()?;
    let tls_offset = validator.find_tls_offset()?;
    let (tls_vaddr, tls_filesz, tls_memsz, tls_align, tls_offset) = match tls_segment {
        Some((vaddr, filesz, memsz, align)) => {
            let offset = tls_offset.ok_or(ElfError::InvalidSegmentOffset)?;
            (vaddr, filesz, memsz, align, Some(offset))
        }
        None => (0, 0, 0, 0, None),
    };

    let process = find_process_vm(process_id);
    if process.is_null() {
        return Err(ElfError::NullPointer);
    }
    let page_dir = unsafe { (*process).page_dir };
    if page_dir.is_null() {
        return Err(ElfError::NullPointer);
    }

    let (min_vaddr, needs_reloc) = calculate_load_offset(segments, code_base);

    unmap_existing_code_region(page_dir, code_base);

    let mut section_mappings: [(u64, u64, u64); MAX_LOAD_SEGMENTS] = [(0, 0, 0); MAX_LOAD_SEGMENTS];
    let mut mapping_count = 0usize;
    let mut mapped_pages: u32 = 0;

    for segment in segments.iter() {
        let user_start =
            process_vm_translate_elf_address(segment.original_vaddr, min_vaddr, code_base);
        let user_end = process_vm_translate_elf_address(
            segment.original_vaddr + segment.mem_size,
            min_vaddr,
            code_base,
        );

        if mapping_count < section_mappings.len() {
            section_mappings[mapping_count] = (
                segment.original_vaddr,
                segment.original_vaddr + segment.mem_size,
                user_start,
            );
            mapping_count += 1;
        }

        let pages = load_segment_pages(page_dir, data, segment, user_start, user_end)?;
        mapped_pages = mapped_pages.saturating_add(pages);
    }

    let (tls_tp, tls_pages) =
        setup_tls_block(page_dir, data, tls_offset, tls_filesz, tls_memsz, tls_align)?;
    mapped_pages = mapped_pages.saturating_add(tls_pages);

    if needs_reloc {
        let _ = apply_elf_relocations(
            data.as_ptr(),
            data.len(),
            page_dir,
            &section_mappings[..mapping_count],
        );
    }

    let user_entry = process_vm_translate_elf_address(header.e_entry, min_vaddr, code_base);

    // Compute the user-space address of the program headers. The ELF spec says
    // the phdr table lives at file offset e_phoff, which usually falls inside
    // the first PT_LOAD segment. Walk mapped segments to find the one that
    // contains it.
    let phdr_user_addr = {
        let phoff = header.e_phoff;
        let phdr_end = phoff + (header.e_phnum as u64) * (header.e_phentsize as u64);
        let mut addr = 0u64;
        for seg in segments.iter() {
            let seg_file_end = seg.file_offset + seg.file_size;
            if phoff >= seg.file_offset && phdr_end <= seg_file_end {
                let offset_in_seg = phoff - seg.file_offset;
                let seg_user =
                    process_vm_translate_elf_address(seg.original_vaddr, min_vaddr, code_base);
                addr = seg_user + offset_in_seg;
                break;
            }
        }
        addr
    };

    unsafe {
        (*process).total_pages = (*process).total_pages.saturating_add(mapped_pages);
        *entry_out = user_entry;
    }

    Ok(crate::elf::ElfExecInfo {
        entry: user_entry,
        phdr_addr: phdr_user_addr,
        phent_size: header.e_phentsize,
        phnum: header.e_phnum,
        tls_filesz,
        tls_memsz,
        tls_align,
        tls_vaddr,
        tls_tp,
    })
}

fn calculate_load_offset(segments: &[ValidatedSegment], code_base: u64) -> (u64, bool) {
    let min_vaddr = segments.iter().map(|s| s.original_vaddr).min().unwrap_or(0);

    let needs_reloc = min_vaddr >= KERNEL_VIRTUAL_BASE || min_vaddr != code_base;
    (min_vaddr, needs_reloc)
}

pub fn process_vm_translate_elf_address(addr: u64, min_vaddr: u64, code_base: u64) -> u64 {
    if addr >= KERNEL_VIRTUAL_BASE {
        let offset = addr.wrapping_sub(KERNEL_VIRTUAL_BASE);
        code_base.wrapping_add(offset)
    } else if min_vaddr >= KERNEL_VIRTUAL_BASE {
        let offset = addr.wrapping_sub(min_vaddr);
        code_base.wrapping_add(offset)
    } else if min_vaddr < code_base {
        addr.wrapping_add(code_base.wrapping_sub(min_vaddr))
    } else {
        addr
    }
}

fn unmap_existing_code_region(page_dir: *mut ProcessPageDir, code_base: u64) {
    let code_limit = code_base + 0x100000;
    unmap_user_range(
        page_dir,
        code_base.saturating_sub(0x100000),
        code_limit + 0x100000,
    );
}

fn setup_tls_block(
    page_dir: *mut ProcessPageDir,
    elf_data: &[u8],
    tls_offset: Option<u64>,
    tls_filesz: u64,
    tls_memsz: u64,
    tls_align: u64,
) -> Result<(u64, u32), ElfError> {
    if page_dir.is_null() {
        return Err(ElfError::NullPointer);
    }
    if tls_filesz > tls_memsz {
        return Err(ElfError::FileSizeExceedsMemSize);
    }
    if tls_align != 0 && !tls_align.is_power_of_two() {
        return Err(ElfError::InvalidAlignment);
    }

    let align = tls_align.max(8);
    let tls_size_aligned = if tls_memsz == 0 {
        0
    } else {
        tls_memsz
            .checked_add(align - 1)
            .ok_or(ElfError::SegmentSizeOverflow)?
            & !(align - 1)
    };
    let tcb_size = core::mem::size_of::<u64>() as u64;
    let total_size = tls_size_aligned
        .checked_add(tcb_size)
        .ok_or(ElfError::SegmentSizeOverflow)?;
    let total_size_aligned = align_up(total_size as usize, PAGE_SIZE_4KB as usize) as u64;

    let tls_base = PROCESS_TLS_BASE_VA;
    let tls_end = tls_base
        .checked_add(total_size_aligned)
        .ok_or(ElfError::SegmentSizeOverflow)?;

    unmap_user_range(page_dir, tls_base, tls_end);

    let mut tls_pages = 0u32;
    if map_user_range(
        page_dir,
        tls_base,
        tls_end,
        PageFlags::USER_RW.bits(),
        &mut tls_pages,
    ) != 0
    {
        return Err(ElfError::NullPointer);
    }

    if tls_filesz > 0 {
        let offset = tls_offset.ok_or(ElfError::InvalidSegmentOffset)?;
        let src_end = offset
            .checked_add(tls_filesz)
            .ok_or(ElfError::InvalidSegmentOffset)?;
        if src_end > elf_data.len() as u64 {
            return Err(ElfError::InvalidSegmentOffset);
        }
        let src = &elf_data[offset as usize..src_end as usize];
        write_user_bytes(page_dir, tls_base, src)?;
    }

    if tls_memsz > tls_filesz {
        zero_user_bytes(page_dir, tls_base + tls_filesz, tls_memsz - tls_filesz)?;
    }

    let tp = tls_base + tls_size_aligned;
    write_user_u64(page_dir, tp, tp)?;
    Ok((tp, tls_pages))
}

fn write_user_bytes(
    page_dir: *mut ProcessPageDir,
    dst_addr: u64,
    data: &[u8],
) -> Result<(), ElfError> {
    let mut written = 0usize;
    while written < data.len() {
        let va = dst_addr
            .checked_add(written as u64)
            .ok_or(ElfError::SegmentSizeOverflow)?;
        let page_va = va & !(PAGE_SIZE_4KB - 1);
        let page_off = (va & (PAGE_SIZE_4KB - 1)) as usize;
        let chunk = core::cmp::min(data.len() - written, PAGE_SIZE_4KB as usize - page_off);

        let phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(page_va));
        if phys.is_null() {
            return Err(ElfError::NullPointer);
        }
        let virt = phys.to_virt();
        if virt.is_null() {
            return Err(ElfError::NullPointer);
        }

        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr().add(written),
                virt.as_mut_ptr::<u8>().add(page_off),
                chunk,
            );
        }
        written += chunk;
    }
    Ok(())
}

fn zero_user_bytes(
    page_dir: *mut ProcessPageDir,
    start_addr: u64,
    len: u64,
) -> Result<(), ElfError> {
    let mut zeroed = 0u64;
    while zeroed < len {
        let va = start_addr
            .checked_add(zeroed)
            .ok_or(ElfError::SegmentSizeOverflow)?;
        let page_va = va & !(PAGE_SIZE_4KB - 1);
        let page_off = (va & (PAGE_SIZE_4KB - 1)) as usize;
        let page_remaining = PAGE_SIZE_4KB - page_off as u64;
        let chunk = core::cmp::min(len - zeroed, page_remaining) as usize;

        let phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(page_va));
        if phys.is_null() {
            return Err(ElfError::NullPointer);
        }
        let virt = phys.to_virt();
        if virt.is_null() {
            return Err(ElfError::NullPointer);
        }

        unsafe {
            core::ptr::write_bytes(virt.as_mut_ptr::<u8>().add(page_off), 0, chunk);
        }
        zeroed = zeroed.saturating_add(chunk as u64);
    }
    Ok(())
}

fn write_user_u64(
    page_dir: *mut ProcessPageDir,
    dst_addr: u64,
    value: u64,
) -> Result<(), ElfError> {
    write_user_bytes(page_dir, dst_addr, &value.to_le_bytes())
}

fn load_segment_pages(
    page_dir: *mut ProcessPageDir,
    data: &[u8],
    segment: &ValidatedSegment,
    user_start: u64,
    user_end: u64,
) -> Result<u32, ElfError> {
    let map_flags = if (segment.flags & PF_W) != 0 {
        PageFlags::USER_RW.bits()
    } else {
        PageFlags::USER_RO.bits()
    };

    let page_start = align_down(user_start as usize, PAGE_SIZE_4KB as usize) as u64;
    let page_end = align_up(user_end as usize, PAGE_SIZE_4KB as usize) as u64;

    let mut dst = page_start;
    let mut pages_mapped = 0u32;

    while dst < page_end {
        let existing_phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(dst));
        let phys = if !existing_phys.is_null() {
            if (map_flags & PageFlags::WRITABLE.bits()) != 0 {
                let _ = paging_mark_range_user(
                    page_dir,
                    VirtAddr::new(dst),
                    VirtAddr::new(dst + PAGE_SIZE_4KB),
                    1,
                );
            }
            existing_phys
        } else {
            let new_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
            if new_phys.is_null() {
                return Err(ElfError::NullPointer);
            }
            if map_page_4kb_in_dir(page_dir, VirtAddr::new(dst), new_phys, map_flags) != 0 {
                free_page_frame(new_phys);
                return Err(ElfError::NullPointer);
            }
            pages_mapped += 1;
            new_phys
        };

        let dest_virt = phys.to_virt();
        if dest_virt.is_null() {
            if existing_phys.is_null() {
                free_page_frame(phys);
            }
            return Err(ElfError::NullPointer);
        }

        copy_segment_page_data(data, segment, dst, user_start, dest_virt.as_mut_ptr());

        dst += PAGE_SIZE_4KB;
    }

    Ok(pages_mapped)
}

fn copy_segment_page_data(
    data: &[u8],
    segment: &ValidatedSegment,
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
pub fn create_process_vm() -> u32 {
    let layout = aslr::randomize_process_layout(&DEFAULT_PROCESS_LAYOUT);
    let mut manager = VM_MANAGER.lock();
    if manager.num_processes >= MAX_PROCESSES as u32 {
        klog_info!("create_process_vm: Maximum processes reached");
        return INVALID_PROCESS_ID;
    }
    let mut process_ptr: *mut ProcessVm = ptr::null_mut();
    for i in 0..MAX_PROCESSES {
        if manager.processes[i].process_id == INVALID_PROCESS_ID {
            process_ptr = &manager.processes[i] as *const _ as *mut ProcessVm;
            break;
        }
    }
    if process_ptr.is_null() {
        klog_info!("create_process_vm: No free process slots available");
        return INVALID_PROCESS_ID;
    }

    let pml4_phys = alloc_page_frame(0);
    if pml4_phys.is_null() {
        klog_info!("create_process_vm: Failed to allocate PML4");
        return INVALID_PROCESS_ID;
    }
    let pml4 = pml4_phys.to_virt().as_mut_ptr::<PageTable>();
    if pml4.is_null() {
        klog_info!("create_process_vm: No HHDM/identity map available for PML4");
        free_page_frame(pml4_phys);
        return INVALID_PROCESS_ID;
    }
    unsafe {
        (*pml4).zero();
    }

    let process_id = manager.next_process_id;
    manager.next_process_id += 1;

    let page_dir_ptr = kmalloc(core::mem::size_of::<ProcessPageDir>()) as *mut ProcessPageDir;
    if page_dir_ptr.is_null() {
        klog_info!("create_process_vm: Failed to allocate page directory");
        free_page_frame(pml4_phys);
        return INVALID_PROCESS_ID;
    }
    unsafe {
        (*page_dir_ptr).pml4 = pml4;
        (*page_dir_ptr).pml4_phys = pml4_phys;
        (*page_dir_ptr).ref_count = 1;
        (*page_dir_ptr).process_id = process_id;
        (*page_dir_ptr).next = ptr::null_mut();
        (*page_dir_ptr).kernel_mapping_gen = 0;
    }

    unsafe {
        paging_copy_kernel_mappings((*page_dir_ptr).pml4);
    }

    unsafe {
        let proc = &mut *process_ptr;
        proc.process_id = process_id;
        proc.page_dir = page_dir_ptr;
        proc.vma_tree.clear();
        proc.code_start = layout.code_start;
        proc.data_start = layout.data_start;
        proc.heap_start = layout.heap_start;
        proc.heap_end = layout.heap_start;
        proc.stack_start = layout.stack_top - layout.stack_size;
        proc.stack_end = layout.stack_top;
        proc.total_pages = 1;
        proc.flags = 0;
        proc.next = manager.process_list;
        if add_vma_to_process(
            process_ptr,
            proc.code_start,
            proc.data_start,
            VmaFlags::USER_CODE,
        ) != 0
            || add_vma_to_process(
                process_ptr,
                proc.data_start,
                proc.heap_start,
                VmaFlags::USER_DATA,
            ) != 0
            || add_vma_to_process(
                process_ptr,
                proc.stack_start,
                proc.stack_end,
                VmaFlags::READ | VmaFlags::WRITE | VmaFlags::USER | VmaFlags::STACK,
            ) != 0
        {
            klog_info!("create_process_vm: Failed to seed initial VMAs");
            teardown_process_mappings(process_ptr);
            free_page_frame((*page_dir_ptr).pml4_phys);
            kfree(page_dir_ptr as *mut _);
            proc.page_dir = ptr::null_mut();
            proc.process_id = INVALID_PROCESS_ID;
            return INVALID_PROCESS_ID;
        }

        let stack_vma_flags = VmaFlags::READ | VmaFlags::WRITE | VmaFlags::USER | VmaFlags::STACK;
        let stack_map_flags = stack_vma_flags.to_page_flags().bits();
        let mut stack_pages: u32 = 0;
        if map_user_range(
            proc.page_dir,
            proc.stack_start,
            proc.stack_end,
            stack_map_flags,
            &mut stack_pages,
        ) != 0
        {
            klog_info!("create_process_vm: Failed to map process stack");
            teardown_process_mappings(process_ptr);
            free_page_frame((*page_dir_ptr).pml4_phys);
            kfree(page_dir_ptr as *mut _);
            proc.page_dir = ptr::null_mut();
            proc.process_id = INVALID_PROCESS_ID;
            return INVALID_PROCESS_ID;
        }
        proc.total_pages += stack_pages;

        // Map a single zero page to tolerate benign null accesses in early userland.
        // This keeps user tasks from immediately faulting on startup before they
        // can report richer diagnostics.
        let mut null_pages: u32 = 0;
        if map_user_range(
            proc.page_dir,
            0,
            PAGE_SIZE_4KB,
            PageFlags::USER_RW.bits(),
            &mut null_pages,
        ) == 0
        {
            let _ = add_vma_to_process(
                process_ptr,
                0,
                PAGE_SIZE_4KB,
                VmaFlags::READ | VmaFlags::WRITE | VmaFlags::USER,
            );
            proc.total_pages += null_pages;
        } else {
            klog_info!("create_process_vm: Failed to map null page for user task");
        }

        manager.process_list = process_ptr;
        manager.num_processes += 1;
        klog_info!("Created process VM space for PID {}", process_id);
    }
    process_id
}
pub fn destroy_process_vm(process_id: u32) -> c_int {
    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return 0;
    }
    unsafe {
        if (*process_ptr).process_id == INVALID_PROCESS_ID {
            return 0;
        }
        klog_info!("Destroying process VM space for PID {}", process_id);
    }

    unsafe {
        klog_debug!(
            "destroy_process_vm({}): teardown_process_mappings",
            process_id
        );
        teardown_process_mappings(process_ptr);
        klog_debug!("destroy_process_vm({}): paging_free_user_space", process_id);
        paging_free_user_space((*process_ptr).page_dir);
        if !(*process_ptr).page_dir.is_null() {
            if !(*(*process_ptr).page_dir).pml4_phys.is_null() {
                free_page_frame((*(*process_ptr).page_dir).pml4_phys);
            }
            klog_debug!("destroy_process_vm({}): kfree(page_dir)", process_id);
            kfree((*process_ptr).page_dir as *mut _);
            (*process_ptr).page_dir = ptr::null_mut();
        }
        klog_debug!(
            "destroy_process_vm({}): page table cleanup done",
            process_id
        );
    }

    let mut manager = VM_MANAGER.lock();
    unsafe {
        if manager.process_list == process_ptr {
            manager.process_list = (*process_ptr).next;
        } else {
            let mut current = manager.process_list;
            while !current.is_null() && (*current).next != process_ptr {
                current = (*current).next;
            }
            if !current.is_null() {
                (*current).next = (*process_ptr).next;
            }
        }
        if manager.active_process == process_ptr {
            manager.active_process = ptr::null_mut();
        }
        (*process_ptr).process_id = INVALID_PROCESS_ID;
        (*process_ptr).next = ptr::null_mut();
        (*process_ptr).total_pages = 0;
        (*process_ptr).flags = 0;
        manager.num_processes = manager.num_processes.saturating_sub(1);
    }
    0
}
pub fn process_vm_alloc(process_id: u32, size: u64, flags: u32) -> u64 {
    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return 0;
    }
    let process = unsafe { &mut *process_ptr };
    let size_aligned = (size + PAGE_SIZE_4KB - 1) & !(PAGE_SIZE_4KB - 1);
    if size_aligned == 0 {
        return 0;
    }
    let start_addr = process.heap_end;
    let end_addr = start_addr + size_aligned;
    if end_addr > DEFAULT_PROCESS_LAYOUT.heap_max {
        klog_info!("process_vm_alloc: Heap overflow");
        return 0;
    }

    let mut vma_flags =
        VmaFlags::READ | VmaFlags::USER | VmaFlags::HEAP | VmaFlags::LAZY | VmaFlags::ANON;
    if flags & PageFlags::WRITABLE.bits() as u32 != 0 {
        vma_flags |= VmaFlags::WRITE;
    }

    if add_vma_to_process(process_ptr, start_addr, end_addr, vma_flags) != 0 {
        klog_info!("process_vm_alloc: Failed to record VMA");
        return 0;
    }

    process.heap_end = end_addr;
    start_addr
}
pub fn process_vm_free(process_id: u32, vaddr: u64, size: u64) -> c_int {
    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() || size == 0 {
        return -1;
    }
    let process = unsafe { &mut *process_ptr };

    let start = vaddr & !(PAGE_SIZE_4KB - 1);
    let end = (vaddr + size + PAGE_SIZE_4KB - 1) & !(PAGE_SIZE_4KB - 1);
    if !vma_range_valid(start, end) {
        klog_info!("process_vm_free: Invalid or unaligned range");
        return -1;
    }

    let vma = find_vma_covering(process_ptr, start, end);
    if vma.is_null() {
        klog_info!("process_vm_free: Range not covered by a VMA");
        return -1;
    }

    let freed = unmap_and_free_range(process_ptr, start, end);

    unsafe {
        let tree = &mut (*process_ptr).vma_tree;
        if start == (*vma).start && end == (*vma).end {
            tree.remove((*vma).start, (*vma).end);
        } else if start == (*vma).start {
            tree.set_start(vma, end);
        } else if end == (*vma).end {
            tree.set_end(vma, start);
        } else {
            let right_start = end;
            let right_end = (*vma).end;
            let flags = (*vma).flags;
            tree.set_end(vma, start);
            if tree.insert(right_start, right_end, flags).is_null() {
                klog_info!("process_vm_free: Failed to create right split VMA");
                return -1;
            }
        }
        if process.total_pages >= freed {
            process.total_pages -= freed;
        } else {
            process.total_pages = 0;
        }
        if process.heap_end == end && end > process.heap_start {
            process.heap_end = start;
        }
    }
    0
}
fn collect_active_pids() -> [u32; MAX_PROCESSES] {
    let manager = VM_MANAGER.lock();
    let mut pids = [INVALID_PROCESS_ID; MAX_PROCESSES];
    for (i, proc) in manager.processes.iter().enumerate() {
        if proc.process_id != INVALID_PROCESS_ID {
            pids[i] = proc.process_id;
        }
    }
    pids
}

pub fn init_process_vm() -> c_int {
    for pid in collect_active_pids() {
        if pid != INVALID_PROCESS_ID {
            destroy_process_vm(pid);
        }
    }

    let mut manager = VM_MANAGER.lock();
    manager.num_processes = 0;
    manager.next_process_id = 1;
    manager.active_process = ptr::null_mut();
    manager.process_list = ptr::null_mut();
    for i in 0..MAX_PROCESSES {
        manager.processes[i].reset();
    }
    klog_info!("Process VM manager initialized");

    0
}
pub fn get_process_vm_stats(total_processes: *mut u32, active_processes: *mut u32) {
    let manager = VM_MANAGER.lock();
    unsafe {
        if !total_processes.is_null() {
            *total_processes = MAX_PROCESSES as u32;
        }
        if !active_processes.is_null() {
            *active_processes = manager.num_processes;
        }
    }
}
pub fn get_current_process_id() -> u32 {
    let manager = VM_MANAGER.lock();
    if manager.active_process.is_null() {
        0
    } else {
        unsafe { (*manager.active_process).process_id }
    }
}

pub fn process_vm_get_vma_flags(process_id: u32, addr: u64) -> Option<VmaFlags> {
    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return None;
    }

    let aligned_addr = addr & !(PAGE_SIZE_4KB - 1);
    let vma = unsafe { (*process_ptr).vma_tree.find_containing(aligned_addr) };
    if vma.is_null() {
        return None;
    }

    Some(unsafe { (*vma).flags })
}

pub fn process_vm_increment_pages(process_id: u32, count: u32) {
    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return;
    }

    unsafe {
        (*process_ptr).total_pages = (*process_ptr).total_pages.saturating_add(count);
    }
}

pub fn process_vm_get_stack_top(process_id: u32) -> u64 {
    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return 0;
    }
    unsafe { (*process_ptr).stack_end }
}

pub fn process_vm_reset_stack(process_id: u32) -> c_int {
    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return -1;
    }
    let (page_dir, stack_start, stack_end) = unsafe {
        let p = &*process_ptr;
        (p.page_dir, p.stack_start, p.stack_end)
    };
    if page_dir.is_null() || stack_end <= stack_start {
        return -1;
    }

    unmap_user_range(page_dir, stack_start, stack_end);

    let stack_flags =
        (VmaFlags::READ | VmaFlags::WRITE | VmaFlags::USER | VmaFlags::STACK).to_page_flags();
    let mut pages: u32 = 0;
    if map_user_range(
        page_dir,
        stack_start,
        stack_end,
        stack_flags.bits(),
        &mut pages,
    ) != 0
    {
        return -1;
    }

    0
}

pub fn process_vm_brk(process_id: u32, new_brk: u64) -> u64 {
    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return 0;
    }
    let process = unsafe { &mut *process_ptr };
    if new_brk == 0 {
        return process.heap_end;
    }

    let aligned_brk = match new_brk.checked_add(PAGE_SIZE_4KB - 1) {
        Some(v) => v & !(PAGE_SIZE_4KB - 1),
        None => return process.heap_end,
    };

    if aligned_brk < process.heap_start {
        return process.heap_end;
    }

    if aligned_brk > DEFAULT_PROCESS_LAYOUT.heap_max {
        return process.heap_end;
    }

    if aligned_brk > process.heap_end {
        let start_addr = process.heap_end;
        let end_addr = aligned_brk;
        let heap_vma_flags = VmaFlags::READ
            | VmaFlags::WRITE
            | VmaFlags::USER
            | VmaFlags::HEAP
            | VmaFlags::LAZY
            | VmaFlags::ANON;

        if add_vma_to_process(process_ptr, start_addr, end_addr, heap_vma_flags) != 0 {
            return process.heap_end;
        }

        process.heap_end = aligned_brk;
    } else if aligned_brk < process.heap_end {
        let start_addr = aligned_brk;
        let end_addr = process.heap_end;

        let freed = unmap_and_free_range(process_ptr, start_addr, end_addr);
        remove_vma_from_process(process_ptr, start_addr, end_addr);

        if process.total_pages >= freed {
            process.total_pages -= freed;
        } else {
            process.total_pages = 0;
        }
        process.heap_end = aligned_brk;
    }

    process.heap_end
}

// =============================================================================
// mmap / munmap / mprotect
// =============================================================================

/// Convert POSIX mmap prot flags to VmaFlags.
fn prot_to_vma_flags(prot: u64) -> VmaFlags {
    use slopos_abi::syscall::{PROT_EXEC, PROT_READ, PROT_WRITE};

    let mut flags = VmaFlags::USER | VmaFlags::ANON | VmaFlags::LAZY;
    if prot & PROT_READ != 0 {
        flags |= VmaFlags::READ;
    }
    if prot & PROT_WRITE != 0 {
        flags |= VmaFlags::WRITE;
    }
    if prot & PROT_EXEC != 0 {
        flags |= VmaFlags::EXEC;
    }
    flags
}

/// Find a free gap in the process address space within the mmap region.
///
/// Walks the VMA tree starting at `PROCESS_MMAP_START_VA` looking for a gap
/// of at least `size` bytes. Returns the start address of the gap, or 0 on
/// failure.
fn find_mmap_gap(process: *const ProcessVm, size: u64) -> u64 {
    use crate::memory_layout_defs::{PROCESS_MMAP_END_VA, PROCESS_MMAP_START_VA};

    if process.is_null() || size == 0 {
        return 0;
    }

    unsafe {
        let tree = &(*process).vma_tree;
        let mut candidate = PROCESS_MMAP_START_VA;

        let mut cursor = tree.find_first_at_or_after(PROCESS_MMAP_START_VA);

        while !cursor.is_null() {
            let vma_start = (*cursor).start;
            let vma_end = (*cursor).end;

            // If there's enough space before this VMA, use it
            if candidate + size <= vma_start {
                return candidate;
            }

            // Skip past this VMA
            if vma_end > candidate {
                candidate = vma_end;
            }

            cursor = tree.next(cursor);
        }

        // Check space after the last VMA
        if candidate + size <= PROCESS_MMAP_END_VA {
            return candidate;
        }
    }

    0
}

/// Map anonymous memory into the process address space (mmap).
///
/// `addr_hint`: requested address (0 = kernel chooses).
/// `length`: mapping size in bytes (rounded up to page boundary).
/// `prot`: PROT_READ | PROT_WRITE | PROT_EXEC.
/// `flags_val`: MAP_ANONYMOUS | MAP_PRIVATE | MAP_FIXED.
/// `fd`: must be -1 for MAP_ANONYMOUS (file-backed not yet supported).
/// `offset`: must be 0 for MAP_ANONYMOUS.
///
/// Returns the virtual address of the mapping on success, or 0 on failure.
pub fn process_vm_mmap(
    process_id: u32,
    addr_hint: u64,
    length: u64,
    prot: u64,
    flags_val: u64,
    fd: i64,
    offset: u64,
) -> u64 {
    use crate::memory_layout_defs::{PROCESS_MMAP_END_VA, PROCESS_MMAP_START_VA};
    use slopos_abi::syscall::{MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE};

    // Only anonymous private mappings for now
    if flags_val & MAP_ANONYMOUS == 0 {
        klog_info!("process_vm_mmap: Only MAP_ANONYMOUS supported");
        return 0;
    }
    if flags_val & MAP_PRIVATE == 0 {
        klog_info!("process_vm_mmap: Only MAP_PRIVATE supported");
        return 0;
    }
    if fd != -1 || offset != 0 {
        klog_info!("process_vm_mmap: fd must be -1 and offset 0 for anonymous");
        return 0;
    }
    if length == 0 {
        return 0;
    }

    let size = match length.checked_add(PAGE_SIZE_4KB - 1) {
        Some(v) => v & !(PAGE_SIZE_4KB - 1),
        None => return 0,
    };

    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return 0;
    }

    let is_fixed = flags_val & MAP_FIXED != 0;

    let start_addr = if is_fixed {
        // MAP_FIXED: use exact address, must be page-aligned
        if (addr_hint & (PAGE_SIZE_4KB - 1)) != 0 {
            klog_info!("process_vm_mmap: MAP_FIXED address not page-aligned");
            return 0;
        }
        if addr_hint < PROCESS_MMAP_START_VA
            || addr_hint
                .checked_add(size)
                .map_or(true, |end| end > PROCESS_MMAP_END_VA)
        {
            klog_info!("process_vm_mmap: MAP_FIXED address out of mmap region");
            return 0;
        }
        // Unmap any existing pages in the range first
        let end_addr = addr_hint + size;
        unsafe {
            let tree = &mut (*process_ptr).vma_tree;
            let mut cursor = tree.find_first_at_or_after(addr_hint);
            while !cursor.is_null() && (*cursor).start < end_addr {
                let next = tree.next(cursor);
                let overlap_start = (*cursor).start.max(addr_hint);
                let overlap_end = (*cursor).end.min(end_addr);
                if overlap_start < overlap_end {
                    let freed = unmap_and_free_range(process_ptr, overlap_start, overlap_end);
                    if (*process_ptr).total_pages >= freed {
                        (*process_ptr).total_pages -= freed;
                    } else {
                        (*process_ptr).total_pages = 0;
                    }
                    // Remove or trim the overlapping VMA
                    if (*cursor).start >= addr_hint && (*cursor).end <= end_addr {
                        tree.remove((*cursor).start, (*cursor).end);
                    } else if (*cursor).start < addr_hint && (*cursor).end > end_addr {
                        // Split: trim left, create right
                        let right_start = end_addr;
                        let right_end = (*cursor).end;
                        let flags = (*cursor).flags;
                        tree.set_end(cursor, addr_hint);
                        tree.insert(right_start, right_end, flags);
                    } else if (*cursor).start < addr_hint {
                        tree.set_end(cursor, addr_hint);
                    } else {
                        tree.set_start(cursor, end_addr);
                    }
                }
                cursor = next;
            }
        }
        addr_hint
    } else {
        // Kernel chooses address
        let chosen = find_mmap_gap(process_ptr, size);
        if chosen == 0 {
            klog_info!("process_vm_mmap: No free region found for {} bytes", size);
            return 0;
        }
        chosen
    };

    let end_addr = start_addr + size;
    let vma_flags = prot_to_vma_flags(prot);

    if add_vma_to_process(process_ptr, start_addr, end_addr, vma_flags) != 0 {
        klog_info!("process_vm_mmap: Failed to insert VMA");
        return 0;
    }

    // For the baseline: mappings are lazy (demand-paged), so no physical
    // pages are allocated here. The page fault handler will allocate them
    // when accessed.

    start_addr
}

/// Unmap a previously mmap'd memory region.
///
/// `addr`: start address (must be page-aligned).
/// `length`: length in bytes (rounded up to page size).
///
/// Returns 0 on success, -1 on failure.
pub fn process_vm_munmap(process_id: u32, addr: u64, length: u64) -> i32 {
    if length == 0 || (addr & (PAGE_SIZE_4KB - 1)) != 0 {
        return -1;
    }

    let size = match length.checked_add(PAGE_SIZE_4KB - 1) {
        Some(v) => v & !(PAGE_SIZE_4KB - 1),
        None => return -1,
    };

    let end = match addr.checked_add(size) {
        Some(v) => v,
        None => return -1,
    };

    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return -1;
    }

    // Validate the range falls within user space
    if addr >= crate::memory_layout_defs::USER_SPACE_END_VA
        || end > crate::memory_layout_defs::USER_SPACE_END_VA
    {
        return -1;
    }

    unsafe {
        let tree = &mut (*process_ptr).vma_tree;
        let mut cursor = tree.find_first_at_or_after(addr);
        let mut found_any = false;

        while !cursor.is_null() && (*cursor).start < end {
            let next = tree.next(cursor);
            let vma_start = (*cursor).start;
            let vma_end = (*cursor).end;

            let overlap_start = vma_start.max(addr);
            let overlap_end = vma_end.min(end);

            if overlap_start < overlap_end {
                found_any = true;
                let freed = unmap_and_free_range(process_ptr, overlap_start, overlap_end);
                if (*process_ptr).total_pages >= freed {
                    (*process_ptr).total_pages -= freed;
                } else {
                    (*process_ptr).total_pages = 0;
                }

                if vma_start >= addr && vma_end <= end {
                    // Entire VMA is within range: remove
                    tree.remove(vma_start, vma_end);
                } else if vma_start < addr && vma_end > end {
                    // Range punches a hole: split VMA
                    let right_start = end;
                    let right_end = vma_end;
                    let flags = (*cursor).flags;
                    tree.set_end(cursor, addr);
                    tree.insert(right_start, right_end, flags);
                } else if vma_start < addr {
                    // Trim end
                    tree.set_end(cursor, addr);
                } else {
                    // Trim start
                    tree.set_start(cursor, end);
                }
            }

            cursor = next;
        }

        if !found_any {
            // POSIX says munmap on unmapped region is a no-op, return success
            return 0;
        }
    }

    0
}

/// Change protection on a memory region.
///
/// `addr`: start address (must be page-aligned).
/// `length`: length in bytes (rounded up to page size).
/// `prot`: new protection flags (PROT_READ | PROT_WRITE | PROT_EXEC).
///
/// Returns 0 on success, -1 on failure.
pub fn process_vm_mprotect(process_id: u32, addr: u64, length: u64, prot: u64) -> i32 {
    use crate::paging::paging_update_range_protection;

    if length == 0 || (addr & (PAGE_SIZE_4KB - 1)) != 0 {
        return -1;
    }

    let size = match length.checked_add(PAGE_SIZE_4KB - 1) {
        Some(v) => v & !(PAGE_SIZE_4KB - 1),
        None => return -1,
    };

    let end = match addr.checked_add(size) {
        Some(v) => v,
        None => return -1,
    };

    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return -1;
    }

    // Validate the range falls within user space
    if addr >= crate::memory_layout_defs::USER_SPACE_END_VA
        || end > crate::memory_layout_defs::USER_SPACE_END_VA
    {
        return -1;
    }

    // Build new VMA flags from prot
    let new_prot = prot_to_vma_flags(prot).protection_only();
    let new_page_flags = (new_prot | VmaFlags::USER).to_page_flags();

    unsafe {
        let page_dir = (*process_ptr).page_dir;
        if page_dir.is_null() {
            return -1;
        }

        // Verify the entire range is covered by VMAs
        let vma = find_vma_covering(process_ptr, addr, end);
        if vma.is_null() {
            // Range not fully covered -- check if there's partial coverage
            // For baseline: require full coverage by a single VMA
            klog_info!("process_vm_mprotect: Range not covered by VMA");
            return -1;
        }

        // Update VMA protection flags (preserve state flags)
        let state = (*vma).flags.state_only();
        (*vma).flags = new_prot | VmaFlags::USER | state;

        // Update page table entries for already-mapped pages
        paging_update_range_protection(
            page_dir,
            VirtAddr::new(addr),
            VirtAddr::new(end),
            new_page_flags,
        );
    }

    0
}

/// Clone address space with COW for fork(). Returns child PID or INVALID_PROCESS_ID.
pub fn process_vm_clone_cow(parent_id: u32) -> u32 {
    let parent_ptr = find_process_vm(parent_id);
    if parent_ptr.is_null() {
        klog_info!(
            "process_vm_clone_cow: Parent process {} not found",
            parent_id
        );
        return INVALID_PROCESS_ID;
    }

    let parent = unsafe { &*parent_ptr };
    if parent.page_dir.is_null() {
        klog_info!("process_vm_clone_cow: Parent has no page directory");
        return INVALID_PROCESS_ID;
    }

    let mut manager = VM_MANAGER.lock();
    if manager.num_processes >= MAX_PROCESSES as u32 {
        klog_info!("process_vm_clone_cow: Maximum processes reached");
        return INVALID_PROCESS_ID;
    }

    let mut child_ptr: *mut ProcessVm = ptr::null_mut();
    for i in 0..MAX_PROCESSES {
        if manager.processes[i].process_id == INVALID_PROCESS_ID {
            child_ptr = &manager.processes[i] as *const _ as *mut ProcessVm;
            break;
        }
    }
    if child_ptr.is_null() {
        klog_info!("process_vm_clone_cow: No free process slots");
        return INVALID_PROCESS_ID;
    }

    let pml4_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if pml4_phys.is_null() {
        klog_info!("process_vm_clone_cow: Failed to allocate PML4");
        return INVALID_PROCESS_ID;
    }
    let pml4 = pml4_phys.to_virt().as_mut_ptr::<PageTable>();
    if pml4.is_null() {
        klog_info!("process_vm_clone_cow: No HHDM mapping for PML4");
        free_page_frame(pml4_phys);
        return INVALID_PROCESS_ID;
    }
    unsafe {
        (*pml4).zero();
    }

    let child_id = manager.next_process_id;
    manager.next_process_id += 1;

    let child_page_dir = kmalloc(core::mem::size_of::<ProcessPageDir>()) as *mut ProcessPageDir;
    if child_page_dir.is_null() {
        klog_info!("process_vm_clone_cow: Failed to allocate page directory struct");
        free_page_frame(pml4_phys);
        return INVALID_PROCESS_ID;
    }
    unsafe {
        (*child_page_dir).pml4 = pml4;
        (*child_page_dir).pml4_phys = pml4_phys;
        (*child_page_dir).ref_count = 1;
        (*child_page_dir).process_id = child_id;
        (*child_page_dir).next = ptr::null_mut();
        (*child_page_dir).kernel_mapping_gen = 0;
    }

    unsafe {
        paging_copy_kernel_mappings((*child_page_dir).pml4);
    }

    let child = unsafe { &mut *child_ptr };
    child.process_id = child_id;
    child.page_dir = child_page_dir;
    child.vma_tree.clear();
    child.code_start = parent.code_start;
    child.data_start = parent.data_start;
    child.heap_start = parent.heap_start;
    child.heap_end = parent.heap_end;
    child.stack_start = parent.stack_start;
    child.stack_end = parent.stack_end;
    child.total_pages = 0;
    child.flags = parent.flags;
    child.next = manager.process_list;

    drop(manager);

    let mut cow_pages: u32 = 0;
    let mut clone_failed = false;

    unsafe {
        let parent_tree = &(*parent_ptr).vma_tree;
        let child_tree = &mut (*child_ptr).vma_tree;

        let mut cursor = parent_tree.first();
        while !cursor.is_null() {
            let vma = &*cursor;
            let vma_start = vma.start;
            let vma_end = vma.end;
            let child_vma_flags = vma.flags | VmaFlags::COW;

            let child_vma = child_tree.insert(vma_start, vma_end, child_vma_flags);
            if child_vma.is_null() {
                klog_info!(
                    "process_vm_clone_cow: Failed to insert VMA [{:#x}, {:#x})",
                    vma_start,
                    vma_end
                );
                clone_failed = true;
                break;
            }

            let mut addr = vma_start;
            while addr < vma_end {
                let vaddr = VirtAddr::new(addr);
                let phys = virt_to_phys_in_dir(parent.page_dir, vaddr);

                if !phys.is_null() {
                    let flags_opt = paging_get_pte_flags(parent.page_dir, vaddr);
                    if let Some(flags) = flags_opt {
                        if !flags.contains(PageFlags::USER) {
                            addr += PAGE_SIZE_4KB;
                            continue;
                        }

                        if flags.contains(PageFlags::WRITABLE) {
                            paging_mark_cow(parent.page_dir, vaddr);
                        }

                        page_frame_inc_ref(phys);

                        let child_flags = (flags.bits() & !PageFlags::WRITABLE.bits())
                            | PageFlags::COW.bits()
                            | PageFlags::USER.bits()
                            | PageFlags::PRESENT.bits();

                        if map_page_4kb_in_dir(child_page_dir, vaddr, phys, child_flags) != 0 {
                            klog_info!("process_vm_clone_cow: Failed to map page {:#x}", addr);
                            free_page_frame(phys);
                            clone_failed = true;
                            break;
                        }

                        cow_pages += 1;
                    }
                }

                addr += PAGE_SIZE_4KB;
            }

            if clone_failed {
                break;
            }

            cursor = parent_tree.next(cursor);
        }
    }

    // paging_mark_cow defers TLB invalidation — flush once for all COW pages.
    if cow_pages > 0 {
        tlb::flush_all();
    }

    if clone_failed {
        klog_info!("process_vm_clone_cow: Clone failed, cleaning up");
        unsafe {
            teardown_process_mappings(child_ptr);
            paging_free_user_space(child_page_dir);
            if !(*child_page_dir).pml4_phys.is_null() {
                free_page_frame((*child_page_dir).pml4_phys);
            }
            kfree(child_page_dir as *mut _);
            (*child_ptr).reset();
        }
        return INVALID_PROCESS_ID;
    }

    let mut manager = VM_MANAGER.lock();
    unsafe {
        (*child_ptr).total_pages = cow_pages;
        manager.process_list = child_ptr;
        manager.num_processes += 1;
    }

    klog_info!(
        "process_vm_clone_cow: Cloned PID {} -> PID {} ({} COW pages)",
        parent_id,
        child_id,
        cow_pages
    );

    child_id
}

pub unsafe fn process_vm_force_unlock() {
    VM_MANAGER.force_unlock();
}

/// Force-unlock the VM manager AND mark it as poisoned.
/// Called from panic recovery to signal that VM state may be
/// inconsistent. Check `process_vm_is_poisoned()` before trusting state.
pub unsafe fn process_vm_poison_unlock() {
    VM_MANAGER.poison_unlock();
}

/// Returns true if the VM manager was force-unlocked during panic recovery.
pub fn process_vm_is_poisoned() -> bool {
    VM_MANAGER.is_poisoned()
}

/// Clear the VM manager's poisoned state after reinitialization.
pub fn process_vm_clear_poison() {
    VM_MANAGER.clear_poison();
}
