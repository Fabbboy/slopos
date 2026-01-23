use core::ffi::c_int;
use core::ptr;

use slopos_abi::addr::VirtAddr;
use slopos_lib::{IrqMutex, align_down, align_up, klog_info};

use crate::aslr;
use crate::elf::{ElfError, ElfValidator, MAX_LOAD_SEGMENTS, PF_W, ValidatedSegment};
use crate::hhdm::PhysAddrHhdm;
use crate::kernel_heap::{kfree, kmalloc};
use crate::memory_layout::mm_get_process_layout;
use crate::mm_constants::{INVALID_PROCESS_ID, MAX_PROCESSES, PAGE_SIZE_4KB, PageFlags};
use crate::page_alloc::{
    ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame, page_frame_can_free, page_frame_inc_ref,
};
use crate::paging::{
    PageTable, ProcessPageDir, map_page_4kb_in_dir, paging_copy_kernel_mappings,
    paging_free_user_space, paging_get_pte_flags, paging_mark_cow, paging_mark_range_user,
    unmap_page_in_dir, virt_to_phys_in_dir,
};
use crate::vma_flags::VmaFlags;
use crate::vma_tree::{VmaNode, VmaTree};

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
        let phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(current));
        if !phys.is_null() {
            unmap_page_in_dir(page_dir, VirtAddr::new(current));
            if page_frame_can_free(phys) != 0 {
                free_page_frame(phys);
            }
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
        let phys = virt_to_phys_in_dir(page_dir, VirtAddr::new(addr));
        if !phys.is_null() && page_frame_can_free(phys) != 0 {
            unmap_page_in_dir(page_dir, VirtAddr::new(addr));
            free_page_frame(phys);
        }
        addr += PAGE_SIZE_4KB;
    }
}

pub fn unmap_user_range_pub(page_dir: *mut ProcessPageDir, start_addr: u64, end_addr: u64) {
    unmap_user_range(page_dir, start_addr, end_addr);
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
            let phys = virt_to_phys_in_dir((*process).page_dir, VirtAddr::new(addr));
            if !phys.is_null() {
                let was_allocated = page_frame_can_free(phys) != 0;
                unmap_page_in_dir((*process).page_dir, VirtAddr::new(addr));
                if was_allocated {
                    freed += 1;
                }
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

fn map_user_sections(page_dir: *mut ProcessPageDir) -> c_int {
    if page_dir.is_null() {
        return -1;
    }

    // User programs are now loaded as separate ELF binaries via process_vm_load_elf(),
    // so we no longer need to map embedded sections from the kernel binary.
    // This function is kept for compatibility but does nothing.
    // The embedded .user_* sections in the kernel are no longer used for user programs.
    0
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

pub fn process_vm_load_elf(
    process_id: u32,
    payload: *const u8,
    payload_len: usize,
    entry_out: *mut u64,
) -> c_int {
    if payload.is_null() || process_id == INVALID_PROCESS_ID {
        return -1;
    }

    let data = unsafe { core::slice::from_raw_parts(payload, payload_len) };

    match process_vm_load_elf_validated(process_id, data, entry_out) {
        Ok(()) => 0,
        Err(e) => {
            klog_info!("ELF load failed: {}", e);
            -1
        }
    }
}

fn process_vm_load_elf_validated(
    process_id: u32,
    data: &[u8],
    entry_out: *mut u64,
) -> Result<(), ElfError> {
    let code_base = crate::mm_constants::PROCESS_CODE_START_VA;

    let validator = ElfValidator::new(data)?.with_load_base(code_base);
    let header = validator.header();

    let (segments, segment_count) = validator.validate_load_segments()?;
    let segments = &segments[..segment_count];

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
        let user_start = translate_address(segment.original_vaddr, min_vaddr, code_base);
        let user_end = translate_address(
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

    if needs_reloc {
        let _ = apply_elf_relocations(
            data.as_ptr(),
            data.len(),
            page_dir,
            &section_mappings[..mapping_count],
        );
    }

    let user_entry = translate_address(header.e_entry, min_vaddr, code_base);

    unsafe {
        (*process).total_pages = (*process).total_pages.saturating_add(mapped_pages);
        if !entry_out.is_null() {
            *entry_out = user_entry;
        }
    }

    Ok(())
}

const KERNEL_BASE: u64 = 0xFFFF_FFFF_8000_0000;

fn calculate_load_offset(segments: &[ValidatedSegment], code_base: u64) -> (u64, bool) {
    let min_vaddr = segments.iter().map(|s| s.original_vaddr).min().unwrap_or(0);

    let needs_reloc = min_vaddr >= KERNEL_BASE || min_vaddr != code_base;
    (min_vaddr, needs_reloc)
}

fn translate_address(addr: u64, min_vaddr: u64, code_base: u64) -> u64 {
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

fn unmap_existing_code_region(page_dir: *mut ProcessPageDir, code_base: u64) {
    let code_limit = code_base + 0x100000;
    unmap_user_range(
        page_dir,
        code_base.saturating_sub(0x100000),
        code_limit + 0x100000,
    );
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
    let base_layout = unsafe { &*mm_get_process_layout() };
    let layout = aslr::randomize_process_layout(base_layout);
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
    }

    unsafe {
        paging_copy_kernel_mappings((*page_dir_ptr).pml4);
        // Map dedicated user sections (text/rodata/data/bss) into the user window.
        if map_user_sections(page_dir_ptr) != 0 {
            kfree(page_dir_ptr as *mut _);
            free_page_frame(pml4_phys);
            return INVALID_PROCESS_ID;
        }
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
        teardown_process_mappings(process_ptr);
        paging_free_user_space((*process_ptr).page_dir);
        if !(*process_ptr).page_dir.is_null() {
            if !(*(*process_ptr).page_dir).pml4_phys.is_null() {
                free_page_frame((*(*process_ptr).page_dir).pml4_phys);
            }
            kfree((*process_ptr).page_dir as *mut _);
            (*process_ptr).page_dir = ptr::null_mut();
        }
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
    let layout = unsafe { &*mm_get_process_layout() };

    let size_aligned = (size + PAGE_SIZE_4KB - 1) & !(PAGE_SIZE_4KB - 1);
    if size_aligned == 0 {
        return 0;
    }
    let start_addr = process.heap_end;
    let end_addr = start_addr + size_aligned;
    if end_addr > layout.heap_max {
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

pub fn process_vm_brk(process_id: u32, new_brk: u64) -> u64 {
    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return 0;
    }
    let process = unsafe { &mut *process_ptr };
    let layout = unsafe { &*mm_get_process_layout() };

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

    if aligned_brk > layout.heap_max {
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
