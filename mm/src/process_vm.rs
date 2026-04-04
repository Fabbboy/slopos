extern crate alloc;

use alloc::vec::Vec;
use core::ffi::c_int;
use core::ptr;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_sync::{IrqMutex, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE};
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
use crate::vma_region::{Protection, RegionBacking, RegionPurpose, VmaMap, VmaRegion};
use slopos_abi::task::INVALID_PROCESS_ID;

/// Per-process VM state, protected by the per-slot lock in `PROCESS_VMS`.
struct ProcessVmInner {
    process_id: u32,
    page_dir: *mut ProcessPageDir,
    vma_map: VmaMap,
    code_start: u64,
    data_start: u64,
    heap_start: u64,
    heap_end: u64,
    stack_start: u64,
    stack_end: u64,
    total_pages: u32,
    flags: u32,
}

unsafe impl Send for ProcessVmInner {}

impl ProcessVmInner {
    const fn new() -> Self {
        Self {
            process_id: INVALID_PROCESS_ID,
            page_dir: ptr::null_mut(),
            vma_map: VmaMap::new(),
            code_start: 0,
            data_start: 0,
            heap_start: 0,
            heap_end: 0,
            stack_start: 0,
            stack_end: 0,
            total_pages: 0,
            flags: 0,
        }
    }

    fn reset(&mut self) {
        self.process_id = INVALID_PROCESS_ID;
        self.page_dir = ptr::null_mut();
        self.vma_map.clear();
        self.code_start = 0;
        self.data_start = 0;
        self.heap_start = 0;
        self.heap_end = 0;
        self.stack_start = 0;
        self.stack_end = 0;
        self.total_pages = 0;
        self.flags = 0;
    }
}

/// Global slot-allocation state: only held during create/destroy/init to
/// manage which slots are in use and the next PID counter.
struct VmSlotAlloc {
    num_processes: u32,
    next_process_id: u32,
}

impl VmSlotAlloc {
    const fn new() -> Self {
        Self {
            num_processes: 0,
            next_process_id: 1,
        }
    }
}

/// Per-process VM locks.  Each slot is independently lockable so that
/// independent processes never contend on each other's VM operations.
static PROCESS_VMS: [IrqMutex<ProcessVmInner>; MAX_PROCESSES] = {
    const INIT: IrqMutex<ProcessVmInner> =
        IrqMutex::new(ProcessVmInner::new(), LOCK_LEVEL_RESOURCE);
    [INIT; MAX_PROCESSES]
};

/// Global slot allocator -- only taken for fork/exit/init to find free slots
/// and update the process count.
static VM_SLOT_ALLOC: IrqMutex<VmSlotAlloc> =
    IrqMutex::new(VmSlotAlloc::new(), LOCK_LEVEL_REGISTRY);

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

/// Find the slot index for a given process ID using a lock-free scan.
///
/// SAFETY: reads `process_id` through `IrqMutex::as_ptr()`.  The field is a
/// naturally-aligned `u32` that is only written under the per-slot lock, so
/// the read is tear-free on x86-64.  The caller must lock `PROCESS_VMS[slot]`
/// before accessing any other field.
fn find_slot_for_pid(process_id: u32) -> Option<usize> {
    if process_id == INVALID_PROCESS_ID {
        return None;
    }
    for i in 0..MAX_PROCESSES {
        // SAFETY: reading a naturally-aligned u32 from a static; see doc above.
        let pid = unsafe { (*PROCESS_VMS[i].as_ptr()).process_id };
        if pid == process_id {
            return Some(i);
        }
    }
    None
}

/// Lock-free page-directory lookup.  The page_dir pointer is only cleared
/// under the per-slot lock during `destroy_process_vm`, which is called after
/// the process has been fully descheduled, so concurrent readers see either
/// the valid pointer or null.
pub fn process_vm_get_page_dir(process_id: u32) -> *mut ProcessPageDir {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return ptr::null_mut(),
    };
    // SAFETY: reading a naturally-aligned pointer from a static.
    unsafe { (*PROCESS_VMS[slot].as_ptr()).page_dir }
}

/// Read the PML4 physical address for a process.  Lock-free for the slot
/// lookup; takes the per-process lock briefly to read pml4_phys so the
/// returned u64 cannot become dangling.
pub fn process_vm_get_cr3_phys(process_id: u32) -> u64 {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return 0,
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return 0;
    }
    let page_dir = guard.page_dir;
    if page_dir.is_null() {
        return 0;
    }
    unsafe { (*page_dir).pml4_phys.as_u64() }
}

pub fn process_vm_find_pid_by_cr3(cr3: u64) -> u32 {
    let cr3_phys = cr3 & !0xFFF;
    if cr3_phys == 0 {
        return INVALID_PROCESS_ID;
    }

    for i in 0..MAX_PROCESSES {
        // SAFETY: lock-free read of naturally-aligned fields.
        let (pid, page_dir) = unsafe {
            let p = &*PROCESS_VMS[i].as_ptr();
            (p.process_id, p.page_dir)
        };
        if pid == INVALID_PROCESS_ID || page_dir.is_null() {
            continue;
        }
        let matches = unsafe { (*page_dir).pml4_phys.as_u64() == cr3_phys };
        if matches {
            return pid;
        }
    }

    INVALID_PROCESS_ID
}

pub fn process_vm_sync_kernel_mappings(process_id: u32) {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return,
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return;
    }
    let page_dir = guard.page_dir;
    if !page_dir.is_null() {
        paging_sync_kernel_mappings(page_dir);
    }
}

/// Insert a VMA into the process address space.
///
/// The caller must guarantee the range does not overlap with existing VMAs
/// (enforced by the gap finder for non-MAP_FIXED mmaps). This matches Linux's
/// design where the gap finder provides the non-overlap guarantee by construction.
///
/// After insertion, adjacent VMAs with compatible metadata are merged
/// automatically inside `VmaMap::insert`.
fn add_vma_to_inner(inner: &mut ProcessVmInner, start: u64, end: u64, region: VmaRegion) -> c_int {
    if !vma_range_valid(start, end) {
        return -1;
    }
    inner.vma_map.insert(start, end, region);
    0
}

/// Convert POSIX mmap prot flags to a `VmaRegion` (anonymous, lazy, user-mode).
fn prot_to_region(prot: u64) -> VmaRegion {
    use slopos_abi::syscall::{PROT_EXEC, PROT_READ, PROT_WRITE};
    VmaRegion {
        protection: Protection {
            read: prot & PROT_READ != 0,
            write: prot & PROT_WRITE != 0,
            exec: prot & PROT_EXEC != 0,
        },
        backing: RegionBacking::Anonymous,
        lazy: true,
        cow: false,
        user: true,
        purpose: RegionPurpose::General,
    }
}

fn unmap_and_free_range_inner(inner: &ProcessVmInner, start: u64, end: u64) -> u32 {
    if inner.page_dir.is_null() || !vma_range_valid(start, end) {
        return 0;
    }
    let mut freed = 0u32;
    let mut addr = start;
    while addr < end {
        let phys = unmap_page_in_dir(inner.page_dir, VirtAddr::new(addr));
        if !phys.is_null() {
            free_page_frame(phys);
            freed += 1;
        }
        addr += PAGE_SIZE_4KB;
    }
    freed
}

fn teardown_inner_mappings(inner: &mut ProcessVmInner) {
    if inner.page_dir.is_null() {
        return;
    }
    let page_dir = inner.page_dir;
    let mut total = inner.total_pages;
    inner.vma_map.drain(|start, end, region| {
        let freed = if region.is_shared() {
            unmap_range_nofree_dir(page_dir, start, end)
        } else {
            unmap_and_free_range_dir(page_dir, start, end)
        };
        total = total.saturating_sub(freed);
        if region.is_shared() && !region.memfd_handle().is_none() {
            crate::memfd::memfd_dec_mapcount(region.memfd_handle().as_usize());
        }
    });
    inner.total_pages = total;
    inner.heap_end = inner.heap_start;
}

/// Unmap and free pages in a range using a raw page_dir pointer.
fn unmap_and_free_range_dir(page_dir: *mut ProcessPageDir, start: u64, end: u64) -> u32 {
    if page_dir.is_null() || !vma_range_valid(start, end) {
        return 0;
    }
    let mut freed = 0u32;
    let mut addr = start;
    while addr < end {
        let phys = unmap_page_in_dir(page_dir, VirtAddr::new(addr));
        if !phys.is_null() {
            free_page_frame(phys);
            freed += 1;
        }
        addr += PAGE_SIZE_4KB;
    }
    freed
}

/// Unmap pages from a page directory WITHOUT freeing the physical frames.
/// Used for shared memfd mappings where pages belong to the MemfdObject.
/// Returns the number of pages unmapped (for total_pages accounting).
fn unmap_range_nofree_dir(page_dir: *mut ProcessPageDir, start: u64, end: u64) -> u32 {
    if page_dir.is_null() || !vma_range_valid(start, end) {
        return 0;
    }
    let mut unmapped = 0u32;
    let mut addr = start;
    while addr < end {
        let phys = unmap_page_in_dir(page_dir, VirtAddr::new(addr));
        if !phys.is_null() {
            // Do NOT free the physical page — it belongs to the memfd.
            unmapped += 1;
        }
        addr += PAGE_SIZE_4KB;
    }
    unmapped
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

    let slot = find_slot_for_pid(process_id).ok_or(ElfError::NullPointer)?;
    let page_dir = {
        let guard = PROCESS_VMS[slot].lock();
        if guard.process_id != process_id {
            return Err(ElfError::NullPointer);
        }
        let pd = guard.page_dir;
        if pd.is_null() {
            return Err(ElfError::NullPointer);
        }
        pd
    };

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

    {
        let mut guard = PROCESS_VMS[slot].lock();
        guard.total_pages = guard.total_pages.saturating_add(mapped_pages);
    }
    *entry_out = user_entry;

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
    // Unmap exactly the code region [code_start, data_start).  The old
    // implementation used wrong arithmetic that extended 1 MB below and
    // above the actual region, potentially unmapping unrelated pages.
    let data_start = crate::memory_layout_defs::PROCESS_DATA_START_VA;
    unmap_user_range(page_dir, code_base, data_start);
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

    // Phase 1: allocate a slot under the global lock.
    let (slot, process_id) = {
        let mut alloc = VM_SLOT_ALLOC.lock();
        if alloc.num_processes >= MAX_PROCESSES as u32 {
            klog_info!("create_process_vm: Maximum processes reached");
            return INVALID_PROCESS_ID;
        }
        let mut found_slot = None;
        for i in 0..MAX_PROCESSES {
            // SAFETY: lock-free read of naturally-aligned u32 to find free slot.
            let pid = unsafe { (*PROCESS_VMS[i].as_ptr()).process_id };
            if pid == INVALID_PROCESS_ID {
                found_slot = Some(i);
                break;
            }
        }
        let slot = match found_slot {
            Some(s) => s,
            None => {
                klog_info!("create_process_vm: No free process slots available");
                return INVALID_PROCESS_ID;
            }
        };
        let process_id = alloc.next_process_id;
        alloc.next_process_id += 1;
        alloc.num_processes += 1;
        (slot, process_id)
    };

    // Phase 2: allocate physical resources (no locks held).
    let pml4_phys = alloc_page_frame(0);
    if pml4_phys.is_null() {
        klog_info!("create_process_vm: Failed to allocate PML4");
        VM_SLOT_ALLOC.lock().num_processes -= 1;
        return INVALID_PROCESS_ID;
    }
    let pml4 = pml4_phys.to_virt().as_mut_ptr::<PageTable>();
    if pml4.is_null() {
        klog_info!("create_process_vm: No HHDM/identity map available for PML4");
        free_page_frame(pml4_phys);
        VM_SLOT_ALLOC.lock().num_processes -= 1;
        return INVALID_PROCESS_ID;
    }
    unsafe {
        (*pml4).zero();
    }

    let page_dir_ptr = kmalloc(core::mem::size_of::<ProcessPageDir>()) as *mut ProcessPageDir;
    if page_dir_ptr.is_null() {
        klog_info!("create_process_vm: Failed to allocate page directory");
        free_page_frame(pml4_phys);
        VM_SLOT_ALLOC.lock().num_processes -= 1;
        return INVALID_PROCESS_ID;
    }
    unsafe {
        (*page_dir_ptr).pml4 = pml4;
        (*page_dir_ptr).pml4_phys = pml4_phys;
        (*page_dir_ptr).ref_count = 1;
        (*page_dir_ptr).process_id = process_id;
        (*page_dir_ptr).next = ptr::null_mut();
        (*page_dir_ptr).kernel_mapping_gen = 0;
        paging_copy_kernel_mappings((*page_dir_ptr).pml4);
    }

    // Phase 3: initialize the per-process slot under its own lock.
    {
        let mut proc = PROCESS_VMS[slot].lock();
        proc.process_id = process_id;
        proc.page_dir = page_dir_ptr;
        proc.vma_map.clear();
        proc.code_start = layout.code_start;
        proc.data_start = layout.data_start;
        proc.heap_start = layout.heap_start;
        proc.heap_end = layout.heap_start;
        proc.stack_start = layout.stack_top - layout.stack_size;
        proc.stack_end = layout.stack_top;
        proc.total_pages = 1;
        proc.flags = 0;

        let code_s = proc.code_start;
        let data_s = proc.data_start;
        let heap_s = proc.heap_start;
        let stack_s = proc.stack_start;
        let stack_e = proc.stack_end;

        let code_region = VmaRegion {
            protection: Protection::RX,
            backing: RegionBacking::Anonymous,
            lazy: false,
            cow: false,
            user: true,
            purpose: RegionPurpose::Code,
        };
        let data_region = VmaRegion {
            protection: Protection::RW,
            backing: RegionBacking::Anonymous,
            lazy: false,
            cow: false,
            user: true,
            purpose: RegionPurpose::Data,
        };
        let stack_region = VmaRegion {
            protection: Protection::RW,
            backing: RegionBacking::Anonymous,
            lazy: false,
            cow: false,
            user: true,
            purpose: RegionPurpose::Stack,
        };

        if add_vma_to_inner(&mut proc, code_s, data_s, code_region) != 0
            || add_vma_to_inner(&mut proc, data_s, heap_s, data_region) != 0
            || add_vma_to_inner(&mut proc, stack_s, stack_e, stack_region) != 0
        {
            klog_info!("create_process_vm: Failed to seed initial VMAs");
            teardown_inner_mappings(&mut proc);
            unsafe {
                free_page_frame((*page_dir_ptr).pml4_phys);
                kfree(page_dir_ptr as *mut _);
            }
            proc.page_dir = ptr::null_mut();
            proc.process_id = INVALID_PROCESS_ID;
            VM_SLOT_ALLOC.lock().num_processes -= 1;
            return INVALID_PROCESS_ID;
        }

        let stack_page_flags = VmaRegion {
            protection: Protection::RW,
            backing: RegionBacking::Anonymous,
            lazy: false,
            cow: false,
            user: true,
            purpose: RegionPurpose::Stack,
        }
        .to_page_flags();
        let mut stack_pages: u32 = 0;
        if map_user_range(
            proc.page_dir,
            proc.stack_start,
            proc.stack_end,
            stack_page_flags.bits(),
            &mut stack_pages,
        ) != 0
        {
            klog_info!("create_process_vm: Failed to map process stack");
            teardown_inner_mappings(&mut proc);
            unsafe {
                free_page_frame((*page_dir_ptr).pml4_phys);
                kfree(page_dir_ptr as *mut _);
            }
            proc.page_dir = ptr::null_mut();
            proc.process_id = INVALID_PROCESS_ID;
            VM_SLOT_ALLOC.lock().num_processes -= 1;
            return INVALID_PROCESS_ID;
        }
        proc.total_pages += stack_pages;

        // Map a single zero page to tolerate benign null accesses in early userland.
        let mut null_pages: u32 = 0;
        if map_user_range(
            proc.page_dir,
            0,
            PAGE_SIZE_4KB,
            PageFlags::USER_RW.bits(),
            &mut null_pages,
        ) == 0
        {
            let null_region = VmaRegion {
                protection: Protection::RW,
                backing: RegionBacking::Anonymous,
                lazy: false,
                cow: false,
                user: true,
                purpose: RegionPurpose::General,
            };
            let _ = add_vma_to_inner(&mut proc, 0, PAGE_SIZE_4KB, null_region);
            proc.total_pages += null_pages;
        } else {
            klog_info!("create_process_vm: Failed to map null page for user task");
        }

        klog_info!("Created process VM space for PID {}", process_id);
    }
    tlb::register_process_tlb(process_id);
    process_id
}

pub fn destroy_process_vm(process_id: u32) -> c_int {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return 0,
    };

    // Read current state under the per-process lock.
    {
        let guard = PROCESS_VMS[slot].lock();
        if guard.process_id == INVALID_PROCESS_ID {
            return 0;
        }
    }
    klog_info!("Destroying process VM space for PID {}", process_id);

    tlb::flush_all_for_process(process_id);
    tlb::unregister_process_tlb(process_id);

    // Teardown under the per-process lock.
    {
        let mut proc = PROCESS_VMS[slot].lock();
        // Re-check after re-acquiring lock.
        if proc.process_id != process_id {
            return 0;
        }

        klog_debug!(
            "destroy_process_vm({}): teardown_process_mappings",
            process_id
        );
        teardown_inner_mappings(&mut proc);
        klog_debug!("destroy_process_vm({}): paging_free_user_space", process_id);
        paging_free_user_space(proc.page_dir);
        if !proc.page_dir.is_null() {
            unsafe {
                if !(*proc.page_dir).pml4_phys.is_null() {
                    free_page_frame((*proc.page_dir).pml4_phys);
                }
                klog_debug!("destroy_process_vm({}): kfree(page_dir)", process_id);
                kfree(proc.page_dir as *mut _);
            }
            proc.page_dir = ptr::null_mut();
        }
        klog_debug!(
            "destroy_process_vm({}): page table cleanup done",
            process_id
        );

        proc.process_id = INVALID_PROCESS_ID;
        proc.total_pages = 0;
        proc.flags = 0;
    }

    // Decrement global count.
    {
        let mut alloc = VM_SLOT_ALLOC.lock();
        alloc.num_processes = alloc.num_processes.saturating_sub(1);
    }
    0
}

pub fn process_vm_alloc(process_id: u32, size: u64, flags: u32) -> u64 {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return 0,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process_id {
        return 0;
    }
    let size_aligned = (size + PAGE_SIZE_4KB - 1) & !(PAGE_SIZE_4KB - 1);
    if size_aligned == 0 {
        return 0;
    }
    let start_addr = proc.heap_end;
    let end_addr = start_addr + size_aligned;
    if end_addr > DEFAULT_PROCESS_LAYOUT.heap_max {
        klog_info!("process_vm_alloc: Heap overflow");
        return 0;
    }

    let heap_region = VmaRegion {
        protection: Protection {
            read: true,
            write: flags & PageFlags::WRITABLE.bits() as u32 != 0,
            exec: false,
        },
        backing: RegionBacking::Anonymous,
        lazy: true,
        cow: false,
        user: true,
        purpose: RegionPurpose::Heap,
    };

    if add_vma_to_inner(&mut proc, start_addr, end_addr, heap_region) != 0 {
        klog_info!("process_vm_alloc: Failed to record VMA");
        return 0;
    }

    proc.heap_end = end_addr;
    start_addr
}

pub fn process_vm_free(process_id: u32, vaddr: u64, size: u64) -> c_int {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return -1,
    };
    if size == 0 {
        return -1;
    }
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process_id {
        return -1;
    }

    let start = vaddr & !(PAGE_SIZE_4KB - 1);
    let end = (vaddr + size + PAGE_SIZE_4KB - 1) & !(PAGE_SIZE_4KB - 1);
    if !vma_range_valid(start, end) {
        klog_info!("process_vm_free: Invalid or unaligned range");
        return -1;
    }

    if proc.vma_map.find_covering(start, end).is_none() {
        klog_info!("process_vm_free: Range not covered by a VMA");
        return -1;
    }

    let freed = unmap_and_free_range_inner(&proc, start, end);

    proc.vma_map
        .remove_range(start, end, |_overlap_start, _overlap_end, _region| {
            // Physical pages already freed above by unmap_and_free_range_inner.
        });

    proc.total_pages = proc.total_pages.saturating_sub(freed);
    if proc.heap_end == end && end > proc.heap_start {
        proc.heap_end = start;
    }

    0
}

fn collect_active_pids() -> [u32; MAX_PROCESSES] {
    let mut pids = [INVALID_PROCESS_ID; MAX_PROCESSES];
    for i in 0..MAX_PROCESSES {
        // SAFETY: lock-free read of naturally-aligned u32.
        let pid = unsafe { (*PROCESS_VMS[i].as_ptr()).process_id };
        if pid != INVALID_PROCESS_ID {
            pids[i] = pid;
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

    {
        let mut alloc = VM_SLOT_ALLOC.lock();
        alloc.num_processes = 0;
        alloc.next_process_id = 1;
    }
    for i in 0..MAX_PROCESSES {
        PROCESS_VMS[i].lock().reset();
    }
    klog_info!("Process VM manager initialized");

    0
}

pub fn get_process_vm_stats(total_processes: *mut u32, active_processes: *mut u32) {
    let alloc = VM_SLOT_ALLOC.lock();
    unsafe {
        if !total_processes.is_null() {
            *total_processes = MAX_PROCESSES as u32;
        }
        if !active_processes.is_null() {
            *active_processes = alloc.num_processes;
        }
    }
}

pub fn get_current_process_id() -> u32 {
    // This function was always racy under SMP with the old global lock (it
    // returned active_process which could change immediately after).
    // With per-process locks there is no meaningful "current" concept at the
    // VM layer -- the scheduler owns that. Return 0 for backwards compat.
    0
}

/// Look up the VMA region at a given address. Returns a cloned region.
pub fn process_vm_get_region(process_id: u32, addr: u64) -> Option<VmaRegion> {
    let slot = find_slot_for_pid(process_id)?;
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return None;
    }

    let aligned_addr = addr & !(PAGE_SIZE_4KB - 1);
    let (_start, _end, region) = guard.vma_map.find_containing(aligned_addr)?;
    Some(region.clone())
}

pub fn process_vm_increment_pages(process_id: u32, count: u32) {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return,
    };
    let mut guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return;
    }
    guard.total_pages = guard.total_pages.saturating_add(count);
}

pub fn process_vm_get_stack_top(process_id: u32) -> u64 {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return 0,
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return 0;
    }
    guard.stack_end
}

pub fn process_vm_reset_stack(process_id: u32) -> c_int {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return -1,
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return -1;
    }
    let page_dir = guard.page_dir;
    let stack_start = guard.stack_start;
    let stack_end = guard.stack_end;
    drop(guard);

    if page_dir.is_null() || stack_end <= stack_start {
        return -1;
    }

    unmap_user_range(page_dir, stack_start, stack_end);

    let stack_page_flags = VmaRegion {
        protection: Protection::RW,
        backing: RegionBacking::Anonymous,
        lazy: false,
        cow: false,
        user: true,
        purpose: RegionPurpose::Stack,
    }
    .to_page_flags();
    let mut pages: u32 = 0;
    if map_user_range(
        page_dir,
        stack_start,
        stack_end,
        stack_page_flags.bits(),
        &mut pages,
    ) != 0
    {
        return -1;
    }

    0
}

pub fn process_vm_brk(process_id: u32, new_brk: u64) -> u64 {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return 0,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process_id {
        return 0;
    }

    if new_brk == 0 {
        return proc.heap_end;
    }

    let aligned_brk = match new_brk.checked_add(PAGE_SIZE_4KB - 1) {
        Some(v) => v & !(PAGE_SIZE_4KB - 1),
        None => return proc.heap_end,
    };

    if aligned_brk < proc.heap_start || aligned_brk > DEFAULT_PROCESS_LAYOUT.heap_max {
        return proc.heap_end;
    }

    if aligned_brk > proc.heap_end {
        let start_addr = proc.heap_end;
        let end_addr = aligned_brk;
        let heap_region = VmaRegion {
            protection: Protection::RW,
            backing: RegionBacking::Anonymous,
            lazy: false,
            cow: false,
            user: true,
            purpose: RegionPurpose::Heap,
        };

        let heap_map_flags = heap_region.to_page_flags().bits();

        if add_vma_to_inner(&mut proc, start_addr, end_addr, heap_region) != 0 {
            return 0;
        }

        let mut pages_mapped: u32 = 0;
        if map_user_range(
            proc.page_dir,
            start_addr,
            end_addr,
            heap_map_flags,
            &mut pages_mapped,
        ) != 0
        {
            proc.vma_map
                .remove_range(start_addr, end_addr, |_, _, _| {});
            return 0;
        }
        proc.total_pages += pages_mapped;
        proc.heap_end = aligned_brk;
    } else if aligned_brk < proc.heap_end {
        let start_addr = aligned_brk;
        let end_addr = proc.heap_end;

        let freed = unmap_and_free_range_inner(&proc, start_addr, end_addr);
        proc.vma_map
            .remove_range(start_addr, end_addr, |_, _, _| {});

        proc.total_pages = proc.total_pages.saturating_sub(freed);
        proc.heap_end = aligned_brk;
    }

    proc.heap_end
}

// =============================================================================
// mmap / munmap / mprotect
// =============================================================================

/// Find a free gap in the process address space within the mmap region.
fn find_mmap_gap_inner(inner: &ProcessVmInner, size: u64) -> u64 {
    use crate::memory_layout_defs::{PROCESS_MMAP_END_VA, PROCESS_MMAP_START_VA};

    if size == 0 {
        return 0;
    }

    inner
        .vma_map
        .find_gap(PROCESS_MMAP_START_VA, PROCESS_MMAP_END_VA, size)
        .unwrap_or(0)
}

/// Map memory into the process address space (mmap).
///
/// Supports anonymous private mappings (existing) and shared memfd mappings (new).
/// For shared mappings, `memfd_handle` must be a valid memfd handle obtained from
/// the syscall handler (which resolves the fd before calling this function).
pub fn process_vm_mmap(
    process_id: u32,
    addr_hint: u64,
    length: u64,
    prot: u64,
    flags_val: u64,
    fd: i64,
    offset: u64,
) -> u64 {
    process_vm_mmap_inner(
        process_id,
        addr_hint,
        length,
        prot,
        flags_val,
        fd,
        offset,
        crate::memfd::MemfdHandle::NONE,
    )
}

/// Extended mmap with optional memfd handle for shared mappings.
pub fn process_vm_mmap_shared(
    process_id: u32,
    addr_hint: u64,
    length: u64,
    prot: u64,
    flags_val: u64,
    offset: u64,
    memfd_handle: crate::memfd::MemfdHandle,
) -> u64 {
    process_vm_mmap_inner(
        process_id,
        addr_hint,
        length,
        prot,
        flags_val,
        -1,
        offset,
        memfd_handle,
    )
}

fn process_vm_mmap_inner(
    process_id: u32,
    addr_hint: u64,
    length: u64,
    prot: u64,
    flags_val: u64,
    fd: i64,
    offset: u64,
    memfd_handle: crate::memfd::MemfdHandle,
) -> u64 {
    use crate::memory_layout_defs::{PROCESS_MMAP_END_VA, PROCESS_MMAP_START_VA};
    use slopos_abi::syscall::{MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE, MAP_SHARED};

    let is_shared = flags_val & MAP_SHARED != 0;
    let is_anonymous = flags_val & MAP_ANONYMOUS != 0;
    let is_private = flags_val & MAP_PRIVATE != 0;

    // Validate flag combinations
    if is_shared && is_private {
        return 0; // Cannot be both
    }
    if is_shared {
        // Shared mapping requires a memfd handle (resolved by syscall handler)
        if memfd_handle.is_none() || offset != 0 {
            klog_info!("process_vm_mmap: MAP_SHARED requires memfd_handle and offset=0");
            return 0;
        }
    } else {
        // Anonymous private (existing path)
        if !is_anonymous || !is_private {
            klog_info!("process_vm_mmap: requires MAP_ANONYMOUS|MAP_PRIVATE or MAP_SHARED");
            return 0;
        }
        if fd != -1 || offset != 0 {
            klog_info!("process_vm_mmap: fd must be -1 and offset 0 for anonymous");
            return 0;
        }
    }

    if length == 0 {
        return 0;
    }

    let size = match length.checked_add(PAGE_SIZE_4KB - 1) {
        Some(v) => v & !(PAGE_SIZE_4KB - 1),
        None => return 0,
    };

    // For shared mappings, validate the memfd and get physical pages
    let shared_info = if is_shared {
        let Some((phys, memfd_size, pages)) = crate::memfd::memfd_get_info(memfd_handle.as_usize())
        else {
            klog_info!("process_vm_mmap: invalid or unsized memfd_handle");
            return 0;
        };
        if size > memfd_size as u64 {
            klog_info!(
                "process_vm_mmap: requested size {} > memfd size {}",
                size,
                memfd_size
            );
            return 0;
        }
        Some((phys, pages))
    } else {
        None
    };

    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return 0,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process_id {
        return 0;
    }

    let is_fixed = flags_val & MAP_FIXED != 0;

    let start_addr = if is_fixed {
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
        let end_addr = addr_hint + size;
        let inner = &mut *proc;
        let page_dir = inner.page_dir;

        // Remove all overlapping VMAs in the fixed range.
        inner
            .vma_map
            .remove_range(addr_hint, end_addr, |overlap_start, overlap_end, region| {
                let freed = if region.is_shared() {
                    unmap_range_nofree_dir(page_dir, overlap_start, overlap_end)
                } else {
                    unmap_and_free_range_dir(page_dir, overlap_start, overlap_end)
                };
                inner.total_pages = inner.total_pages.saturating_sub(freed);
            });

        addr_hint
    } else {
        let chosen = find_mmap_gap_inner(&proc, size);
        if chosen == 0 {
            klog_info!("process_vm_mmap: No free region found for {} bytes", size);
            return 0;
        }
        chosen
    };

    let end_addr = start_addr + size;

    if let Some((phys, _pages)) = shared_info {
        // -- Shared memfd path: eagerly map the memfd's physical pages --
        use slopos_abi::syscall::PROT_WRITE;

        let shared_region = VmaRegion {
            protection: Protection {
                read: prot_to_region(prot).protection.read,
                write: prot_to_region(prot).protection.write,
                exec: prot_to_region(prot).protection.exec,
            },
            backing: RegionBacking::SharedMemfd {
                handle: memfd_handle,
            },
            lazy: false,
            cow: false,
            user: true,
            purpose: RegionPurpose::General,
        };

        let inner = &mut *proc;
        let page_dir = inner.page_dir;
        let page_count = (size / PAGE_SIZE_4KB) as u32;

        // Determine PTE flags from protection
        let pte_flags = if prot & PROT_WRITE != 0 {
            PageFlags::USER_RW.bits()
        } else {
            PageFlags::USER_RO.bits()
        };

        for i in 0..page_count {
            let vaddr = start_addr + (i as u64) * PAGE_SIZE_4KB;
            let paddr = PhysAddr::new(phys.as_u64() + (i as u64) * PAGE_SIZE_4KB);
            if map_page_4kb_in_dir(page_dir, VirtAddr::new(vaddr), paddr, pte_flags) != 0 {
                // Rollback on failure
                for j in 0..i {
                    let rv = start_addr + (j as u64) * PAGE_SIZE_4KB;
                    unmap_page_in_dir(page_dir, VirtAddr::new(rv));
                }
                return 0;
            }
        }

        // Insert the shared VMA
        inner.vma_map.insert(start_addr, end_addr, shared_region);

        inner.total_pages += page_count;

        // Tell the memfd about this mapping
        crate::memfd::memfd_inc_mapcount(memfd_handle.as_usize());

        start_addr
    } else {
        // -- Anonymous private path (existing behavior) --
        let region = prot_to_region(prot);

        if add_vma_to_inner(&mut proc, start_addr, end_addr, region) != 0 {
            klog_info!("process_vm_mmap: Failed to insert VMA");
            return 0;
        }

        start_addr
    }
}

/// Unmap a previously mmap'd memory region.
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

    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return -1,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process_id {
        return -1;
    }

    if addr >= crate::memory_layout_defs::USER_SPACE_END_VA
        || end > crate::memory_layout_defs::USER_SPACE_END_VA
    {
        return -1;
    }

    let inner = &mut *proc;
    let page_dir = inner.page_dir;

    // Pre-scan for EXEC regions -- munmap of executable mappings is forbidden.
    for (s, e, r) in inner.vma_map.iter() {
        if s < end && e > addr && r.protection.exec {
            return -1;
        }
    }

    // Remove range, unmapping pages and handling shared memfd bookkeeping.
    let mut total_freed = 0u32;
    inner
        .vma_map
        .remove_range(addr, end, |overlap_start, overlap_end, region| {
            let freed = if region.is_shared() {
                // Shared: unmap PTEs but do NOT free physical pages
                unmap_range_nofree_dir(page_dir, overlap_start, overlap_end)
            } else {
                // Anonymous: unmap and free as before
                unmap_and_free_range_dir(page_dir, overlap_start, overlap_end)
            };
            total_freed += freed;
            if region.is_shared() && !region.memfd_handle().is_none() {
                crate::memfd::memfd_dec_mapcount(region.memfd_handle().as_usize());
            }
        });
    inner.total_pages = inner.total_pages.saturating_sub(total_freed);

    0
}

/// Change protection on a memory region.
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

    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return -1,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process_id {
        return -1;
    }

    if addr >= crate::memory_layout_defs::USER_SPACE_END_VA
        || end > crate::memory_layout_defs::USER_SPACE_END_VA
    {
        return -1;
    }

    let new_prot = prot_to_region(prot);

    let page_dir = proc.page_dir;
    if page_dir.is_null() {
        return -1;
    }

    let (_vma_start, _vma_end, region) = match proc.vma_map.find_covering_mut(addr, end) {
        Some(v) => v,
        None => {
            klog_info!("process_vm_mprotect: Range not covered by VMA");
            return -1;
        }
    };

    // Update the region's protection bits, preserving other fields.
    region.protection = new_prot.protection;

    // Convert updated region to page flags for the PTE update.
    let new_page_flags = region.to_page_flags();

    paging_update_range_protection(
        page_dir,
        VirtAddr::new(addr),
        VirtAddr::new(end),
        new_page_flags,
    );

    0
}

/// Clone address space with COW for fork(). Returns child PID or INVALID_PROCESS_ID.
pub fn process_vm_clone_cow(parent_id: u32) -> u32 {
    let parent_slot = match find_slot_for_pid(parent_id) {
        Some(s) => s,
        None => {
            klog_info!(
                "process_vm_clone_cow: Parent process {} not found",
                parent_id
            );
            return INVALID_PROCESS_ID;
        }
    };

    // Snapshot parent scalar fields + VMA list under the parent's per-process lock.
    // ProcessVmInner is no longer Copy (BTreeMap), so we collect what we need
    // into plain scalars and a Vec.
    let (
        parent_page_dir,
        parent_code_start,
        parent_data_start,
        parent_heap_start,
        parent_heap_end,
        parent_stack_start,
        parent_stack_end,
        parent_flags,
        parent_vmas,
    ) = {
        let guard = PROCESS_VMS[parent_slot].lock();
        if guard.process_id != parent_id || guard.page_dir.is_null() {
            klog_info!("process_vm_clone_cow: Parent has no page directory");
            return INVALID_PROCESS_ID;
        }
        let vmas: Vec<(u64, u64, VmaRegion)> = guard
            .vma_map
            .iter()
            .map(|(s, e, r)| (s, e, r.clone()))
            .collect();
        (
            guard.page_dir,
            guard.code_start,
            guard.data_start,
            guard.heap_start,
            guard.heap_end,
            guard.stack_start,
            guard.stack_end,
            guard.flags,
            vmas,
        )
    };

    // Phase 1: allocate child slot under global lock.
    let (child_slot, child_id) = {
        let mut alloc = VM_SLOT_ALLOC.lock();
        if alloc.num_processes >= MAX_PROCESSES as u32 {
            klog_info!("process_vm_clone_cow: Maximum processes reached");
            return INVALID_PROCESS_ID;
        }
        let mut found_slot = None;
        for i in 0..MAX_PROCESSES {
            let pid = unsafe { (*PROCESS_VMS[i].as_ptr()).process_id };
            if pid == INVALID_PROCESS_ID {
                found_slot = Some(i);
                break;
            }
        }
        let child_slot = match found_slot {
            Some(s) => s,
            None => {
                klog_info!("process_vm_clone_cow: No free process slots");
                return INVALID_PROCESS_ID;
            }
        };
        let child_id = alloc.next_process_id;
        alloc.next_process_id += 1;
        alloc.num_processes += 1;
        (child_slot, child_id)
    };

    // Phase 2: allocate physical resources (no locks held).
    let pml4_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if pml4_phys.is_null() {
        klog_info!("process_vm_clone_cow: Failed to allocate PML4");
        VM_SLOT_ALLOC.lock().num_processes -= 1;
        return INVALID_PROCESS_ID;
    }
    let pml4 = pml4_phys.to_virt().as_mut_ptr::<PageTable>();
    if pml4.is_null() {
        klog_info!("process_vm_clone_cow: No HHDM mapping for PML4");
        free_page_frame(pml4_phys);
        VM_SLOT_ALLOC.lock().num_processes -= 1;
        return INVALID_PROCESS_ID;
    }
    unsafe {
        (*pml4).zero();
    }

    let child_page_dir = kmalloc(core::mem::size_of::<ProcessPageDir>()) as *mut ProcessPageDir;
    if child_page_dir.is_null() {
        klog_info!("process_vm_clone_cow: Failed to allocate page directory struct");
        free_page_frame(pml4_phys);
        VM_SLOT_ALLOC.lock().num_processes -= 1;
        return INVALID_PROCESS_ID;
    }
    unsafe {
        (*child_page_dir).pml4 = pml4;
        (*child_page_dir).pml4_phys = pml4_phys;
        (*child_page_dir).ref_count = 1;
        (*child_page_dir).process_id = child_id;
        (*child_page_dir).next = ptr::null_mut();
        (*child_page_dir).kernel_mapping_gen = 0;
        paging_copy_kernel_mappings((*child_page_dir).pml4);
    }

    // Phase 3: initialize child slot and perform COW page walk.
    // We hold the child slot lock while building VMA map + page tables.
    // The parent's page_dir pointer is stable (read from snapshot; parent
    // cannot be destroyed while fork is in progress -- scheduler guarantees).
    let mut cow_pages: u32 = 0;
    let mut clone_failed = false;

    {
        let mut child = PROCESS_VMS[child_slot].lock();
        child.process_id = child_id;
        child.page_dir = child_page_dir;
        child.vma_map.clear();
        child.code_start = parent_code_start;
        child.data_start = parent_data_start;
        child.heap_start = parent_heap_start;
        child.heap_end = parent_heap_end;
        child.stack_start = parent_stack_start;
        child.stack_end = parent_stack_end;
        child.total_pages = 0;
        child.flags = parent_flags;

        // Walk parent's VMA list (from snapshot Vec).
        for (vma_start, vma_end, parent_region) in parent_vmas.iter() {
            let vma_start = *vma_start;
            let vma_end = *vma_end;
            let is_shared_vma = parent_region.is_shared();

            let child_region = if is_shared_vma {
                // Shared memfd: inherit directly, no COW
                parent_region.clone()
            } else {
                // Anonymous: mark as COW
                let mut r = parent_region.clone();
                r.cow = true;
                r
            };

            child.vma_map.insert(vma_start, vma_end, child_region);

            if is_shared_vma {
                // Shared memfd: map same physical pages directly, no COW
                let mut addr = vma_start;
                while addr < vma_end {
                    let vaddr = VirtAddr::new(addr);
                    let phys = virt_to_phys_in_dir(parent_page_dir, vaddr);
                    if !phys.is_null() {
                        let flags_opt = paging_get_pte_flags(parent_page_dir, vaddr);
                        if let Some(flags) = flags_opt {
                            // Map with same permissions -- no COW, no ref_count
                            // (pages belong to the memfd, not to processes)
                            if map_page_4kb_in_dir(child_page_dir, vaddr, phys, flags.bits()) != 0 {
                                clone_failed = true;
                                break;
                            }
                            cow_pages += 1;
                        }
                    }
                    addr += PAGE_SIZE_4KB;
                }
                // Increment memfd map_count for the child's mapping
                let memfd_handle = parent_region.memfd_handle();
                if !memfd_handle.is_none() {
                    crate::memfd::memfd_inc_mapcount(memfd_handle.as_usize());
                }
                continue;
            }

            let mut addr = vma_start;
            while addr < vma_end {
                let vaddr = VirtAddr::new(addr);
                let phys = virt_to_phys_in_dir(parent_page_dir, vaddr);

                if !phys.is_null() {
                    let flags_opt = paging_get_pte_flags(parent_page_dir, vaddr);
                    if let Some(flags) = flags_opt {
                        if !flags.contains(PageFlags::USER) {
                            addr += PAGE_SIZE_4KB;
                            continue;
                        }

                        if flags.contains(PageFlags::WRITABLE) {
                            paging_mark_cow(parent_page_dir, vaddr);
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
        }

        if !clone_failed {
            child.total_pages = cow_pages;
        }
    }

    // paging_mark_cow defers TLB invalidation -- flush once for all COW pages.
    if cow_pages > 0 {
        tlb::flush_all();
    }

    if clone_failed {
        klog_info!("process_vm_clone_cow: Clone failed, cleaning up");
        {
            let mut child = PROCESS_VMS[child_slot].lock();
            teardown_inner_mappings(&mut child);
        }
        unsafe {
            paging_free_user_space(child_page_dir);
            if !(*child_page_dir).pml4_phys.is_null() {
                free_page_frame((*child_page_dir).pml4_phys);
            }
            kfree(child_page_dir as *mut _);
        }
        PROCESS_VMS[child_slot].lock().reset();
        VM_SLOT_ALLOC.lock().num_processes -= 1;
        return INVALID_PROCESS_ID;
    }

    klog_info!(
        "process_vm_clone_cow: Cloned PID {} -> PID {} ({} COW pages)",
        parent_id,
        child_id,
        cow_pages
    );

    tlb::register_process_tlb(child_id);

    child_id
}

pub unsafe fn process_vm_force_unlock() {
    VM_SLOT_ALLOC.force_unlock();
    for i in 0..MAX_PROCESSES {
        PROCESS_VMS[i].force_unlock();
    }
}

/// Force-unlock all VM locks AND mark the slot allocator as poisoned.
/// Called from panic recovery to signal that VM state may be
/// inconsistent. Check `process_vm_is_poisoned()` before trusting state.
pub unsafe fn process_vm_poison_unlock() {
    VM_SLOT_ALLOC.poison_unlock();
    for i in 0..MAX_PROCESSES {
        PROCESS_VMS[i].force_unlock();
    }
}

/// Returns true if the slot allocator was force-unlocked during panic recovery.
pub fn process_vm_is_poisoned() -> bool {
    VM_SLOT_ALLOC.is_poisoned()
}

/// Clear the VM slot allocator's poisoned state after reinitialization.
pub fn process_vm_clear_poison() {
    VM_SLOT_ALLOC.clear_poison();
}
