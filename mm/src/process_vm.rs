#![allow(dead_code)]

use core::ffi::{c_char, c_int};
use core::ptr;

use spin::Mutex;

use crate::kernel_heap::{kfree, kmalloc};
use crate::mm_constants::{
    INVALID_PROCESS_ID, MAX_PROCESSES, PAGE_PRESENT, PAGE_SIZE_4KB, PAGE_USER, PAGE_WRITABLE,
    PROCESS_HEAP_MAX_VA, PROCESS_HEAP_START_VA, PROCESS_STACK_SIZE_BYTES, PROCESS_STACK_TOP_VA,
    USER_SPACE_END_VA, USER_SPACE_START_VA,
};
use crate::memory_layout::mm_get_process_layout;
use crate::page_alloc::{alloc_page_frame, free_page_frame, page_frame_can_free, ALLOC_FLAG_ZERO};
use crate::paging::{
    paging_copy_kernel_mappings, paging_free_user_space, paging_get_kernel_directory,
    paging_is_user_accessible, paging_mark_range_user, map_page_4kb_in_dir, unmap_page_in_dir,
    virt_to_phys_in_dir, ProcessPageDir, PageTable,
};
use crate::phys_virt::mm_phys_to_virt;

extern "C" {
    fn klog_printf(level: slopos_lib::klog::KlogLevel, fmt: *const c_char, ...) -> c_int;
}

#[repr(C)]
struct VmArea {
    start_addr: u64,
    end_addr: u64,
    flags: u32,
    ref_count: u32,
    next: *mut VmArea,
}

unsafe impl Send for VmArea {}

impl VmArea {
    fn new(start: u64, end: u64, flags: u32) -> *mut Self {
        let ptr = unsafe { kmalloc(core::mem::size_of::<VmArea>()) as *mut VmArea };
        if ptr.is_null() {
            return ptr::null_mut();
        }
        unsafe {
            (*ptr).start_addr = start;
            (*ptr).end_addr = end;
            (*ptr).flags = flags;
            (*ptr).ref_count = 1;
            (*ptr).next = ptr::null_mut();
        }
        ptr
    }
}

#[repr(C)]
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcessVm {
    process_id: u32,
    page_dir: *mut ProcessPageDir,
    vma_list: *mut VmArea,
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
    const fn empty() -> Self {
        Self {
            process_id: INVALID_PROCESS_ID,
            page_dir: ptr::null_mut(),
            vma_list: ptr::null_mut(),
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
            processes: [ProcessVm::empty(); MAX_PROCESSES],
            num_processes: 0,
            next_process_id: 1,
            active_process: ptr::null_mut(),
            process_list: ptr::null_mut(),
        }
    }
}

static VM_MANAGER: Mutex<VmManager> = Mutex::new(VmManager::new());

fn vma_range_valid(start: u64, end: u64) -> bool {
    start < end && (start & (PAGE_SIZE_4KB - 1)) == 0 && (end & (PAGE_SIZE_4KB - 1)) == 0
}

fn vma_overlaps_range(vma: *const VmArea, start: u64, end: u64) -> bool {
    if vma.is_null() {
        return false;
    }
    unsafe { start < (*vma).end_addr && end > (*vma).start_addr }
}

fn map_user_range(
    page_dir: *mut ProcessPageDir,
    start_addr: u64,
    end_addr: u64,
    map_flags: u64,
    pages_mapped_out: *mut u32,
) -> c_int {
    if page_dir.is_null() {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"map_user_range: Missing page directory\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }
    if (start_addr & (PAGE_SIZE_4KB - 1)) != 0 || (end_addr & (PAGE_SIZE_4KB - 1)) != 0 || end_addr <= start_addr {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"map_user_range: Unaligned or invalid range\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    let mut current = start_addr;
    let mut mapped: u32 = 0;

    while current < end_addr {
        let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
        if phys == 0 {
            unsafe {
                klog_printf(
                    slopos_lib::klog::KlogLevel::Info,
                    b"map_user_range: Physical allocation failed\n\0".as_ptr() as *const c_char,
                );
            }
            rollback_range(page_dir, current, start_addr, &mut mapped);
            if !pages_mapped_out.is_null() {
                unsafe { *pages_mapped_out = 0 };
            }
            return -1;
        }
        if map_page_4kb_in_dir(page_dir, current, phys, map_flags) != 0 {
            unsafe {
                klog_printf(
                    slopos_lib::klog::KlogLevel::Info,
                    b"map_user_range: Virtual mapping failed\n\0".as_ptr() as *const c_char,
                );
            }
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

fn rollback_range(page_dir: *mut ProcessPageDir, mut current: u64, start_addr: u64, mapped: &mut u32) {
    while *mapped > 0 {
        current -= PAGE_SIZE_4KB;
        let phys = virt_to_phys_in_dir(page_dir, current);
        if phys != 0 {
            unmap_page_in_dir(page_dir, current);
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
        let phys = virt_to_phys_in_dir(page_dir, addr);
        if phys != 0 && page_frame_can_free(phys) != 0 {
            unmap_page_in_dir(page_dir, addr);
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

#[no_mangle]
pub extern "C" fn process_vm_get_page_dir(process_id: u32) -> *mut ProcessPageDir {
    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return ptr::null_mut();
    }
    unsafe { (*process_ptr).page_dir }
}

fn add_vma_to_process(process: *mut ProcessVm, start: u64, end: u64, flags: u32) -> c_int {
    if process.is_null() || !vma_range_valid(start, end) {
        return -1;
    }
    unsafe {
        let mut link = &mut (*process).vma_list;
        let mut prev: *mut VmArea = ptr::null_mut();
        while !(*link).is_null() && (**link).start_addr < start {
            prev = *link;
            link = &mut (**link).next;
        }
        let next = *link;
        if !prev.is_null() && vma_overlaps_range(prev, start, end) && (*prev).flags != flags {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"add_vma_to_process: Overlap with incompatible VMA\n\0".as_ptr() as *const c_char,
            );
            return -1;
        }
        if !next.is_null() && vma_overlaps_range(next, start, end) && (*next).flags != flags {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"add_vma_to_process: Overlap with incompatible next VMA\n\0".as_ptr() as *const c_char,
            );
            return -1;
        }

        let mut vma = VmArea::new(start, end, flags);
        if vma.is_null() {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"add_vma_to_process: Failed to allocate VMA\n\0".as_ptr() as *const c_char,
            );
            return -1;
        }

        if !prev.is_null() && (*prev).end_addr == start && (*prev).flags == flags {
            (*prev).end_addr = end;
            kfree(vma as *mut _);
            vma = prev;
        } else {
            (*vma).next = next;
            *link = vma;
        }

        if !(*vma).next.is_null()
            && (*(*vma).next).start_addr == (*vma).end_addr
            && (*(*vma).next).flags == (*vma).flags
        {
            let to_merge = (*vma).next;
            (*vma).end_addr = (*to_merge).end_addr;
            (*vma).next = (*to_merge).next;
            kfree(to_merge as *mut _);
        }
    }
    0
}

fn remove_vma_from_process(process: *mut ProcessVm, start: u64, end: u64) -> c_int {
    if process.is_null() || !vma_range_valid(start, end) {
        return -1;
    }
    unsafe {
        let mut current = &mut (*process).vma_list;
        while !(*current).is_null() {
            let vma = *current;
            if (*vma).start_addr == start && (*vma).end_addr == end {
                *current = (*vma).next;
                (*vma).next = ptr::null_mut();
                kfree(vma as *mut _);
                return 0;
            }
            current = &mut (*vma).next;
        }
    }
    -1
}

fn find_vma_covering(process: *mut ProcessVm, start: u64, end: u64) -> *mut VmArea {
    if process.is_null() || !vma_range_valid(start, end) {
        return ptr::null_mut();
    }
    unsafe {
        let mut cursor = (*process).vma_list;
        while !cursor.is_null() {
            if (*cursor).start_addr <= start && (*cursor).end_addr >= end {
                return cursor;
            }
            cursor = (*cursor).next;
        }
    }
    ptr::null_mut()
}

fn unmap_and_free_range(process: *mut ProcessVm, start: u64, end: u64) -> u32 {
    if process.is_null() || unsafe { (*process).page_dir.is_null() } || !vma_range_valid(start, end) {
        return 0;
    }
    let mut freed = 0u32;
    let mut addr = start;
    unsafe {
        while addr < end {
            let phys = virt_to_phys_in_dir((*process).page_dir, addr);
            if phys != 0 {
                let was_allocated = page_frame_can_free(phys) != 0;
                unmap_page_in_dir((*process).page_dir, addr);
                if was_allocated {
                    freed += 1;
                }
            }
            addr += PAGE_SIZE_4KB;
        }
    }
    freed
}

fn merge_adjacent(process: *mut ProcessVm, mut vma: *mut VmArea) {
    if process.is_null() || vma.is_null() {
        return;
    }
    unsafe {
        let mut cursor = (*process).vma_list;
        let mut prev: *mut VmArea = ptr::null_mut();
        while !cursor.is_null() && cursor != vma {
            prev = cursor;
            cursor = (*cursor).next;
        }

        if !prev.is_null() && (*prev).end_addr == (*vma).start_addr && (*prev).flags == (*vma).flags {
            (*prev).end_addr = (*vma).end_addr;
            (*prev).next = (*vma).next;
            kfree(vma as *mut _);
            vma = prev;
        }

        if !(*vma).next.is_null()
            && (*(*vma).next).start_addr == (*vma).end_addr
            && (*(*vma).next).flags == (*vma).flags
        {
            let n = (*vma).next;
            (*vma).end_addr = (*n).end_addr;
            (*vma).next = (*n).next;
            kfree(n as *mut _);
        }
    }
}

fn teardown_process_mappings(process: *mut ProcessVm) {
    if process.is_null() || unsafe { (*process).page_dir.is_null() } {
        return;
    }
    unsafe {
        let mut cursor = (*process).vma_list;
        while !cursor.is_null() {
            let next = (*cursor).next;
            let freed = unmap_and_free_range(process, (*cursor).start_addr, (*cursor).end_addr);
            if (*process).total_pages >= freed {
                (*process).total_pages -= freed;
            } else {
                (*process).total_pages = 0;
            }
            kfree(cursor as *mut _);
            cursor = next;
        }
        (*process).vma_list = ptr::null_mut();
        (*process).heap_end = (*process).heap_start;
    }
}

#[no_mangle]
pub extern "C" fn create_process_vm() -> u32 {
    let layout = unsafe { &*mm_get_process_layout() };
    let mut manager = VM_MANAGER.lock();
    if manager.num_processes >= MAX_PROCESSES as u32 {
        unsafe { klog_printf(slopos_lib::klog::KlogLevel::Info, b"create_process_vm: Maximum processes reached\n\0".as_ptr() as *const c_char); }
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
        unsafe { klog_printf(slopos_lib::klog::KlogLevel::Info, b"create_process_vm: No free process slots available\n\0".as_ptr() as *const c_char); }
        return INVALID_PROCESS_ID;
    }

    let pml4_phys = alloc_page_frame(0);
    if pml4_phys == 0 {
        unsafe { klog_printf(slopos_lib::klog::KlogLevel::Info, b"create_process_vm: Failed to allocate PML4\n\0".as_ptr() as *const c_char); }
        return INVALID_PROCESS_ID;
    }
    let pml4 = unsafe { mm_phys_to_virt(pml4_phys) as *mut PageTable };
    if pml4.is_null() {
        unsafe { klog_printf(slopos_lib::klog::KlogLevel::Info, b"create_process_vm: No HHDM/identity map available for PML4\n\0".as_ptr() as *const c_char); }
        free_page_frame(pml4_phys);
        return INVALID_PROCESS_ID;
    }
    unsafe {
        (*pml4).entries.fill(0);
    }

    let process_id = manager.next_process_id;
    manager.next_process_id += 1;

    let page_dir_ptr = unsafe { kmalloc(core::mem::size_of::<ProcessPageDir>()) as *mut ProcessPageDir };
    if page_dir_ptr.is_null() {
        unsafe { klog_printf(slopos_lib::klog::KlogLevel::Info, b"create_process_vm: Failed to allocate page directory\n\0".as_ptr() as *const c_char); }
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
        // Expose dedicated user sections (text/rodata/data) if present.
    }

    unsafe {
        let proc = &mut *process_ptr;
        proc.process_id = process_id;
        proc.page_dir = page_dir_ptr;
        proc.vma_list = ptr::null_mut();
        proc.code_start = layout.code_start;
        proc.data_start = layout.data_start;
        proc.heap_start = layout.heap_start;
        proc.heap_end = layout.heap_start;
        proc.stack_start = layout.stack_top - layout.stack_size;
        proc.stack_end = layout.stack_top;
        proc.total_pages = 1;
        proc.flags = 0;
        proc.next = manager.process_list;
        if add_vma_to_process(process_ptr, proc.code_start, proc.data_start, PAGE_PRESENT as u32 | PAGE_USER as u32 | 0x04) != 0
            || add_vma_to_process(process_ptr, proc.data_start, proc.heap_start, PAGE_PRESENT as u32 | PAGE_USER as u32 | PAGE_WRITABLE as u32) != 0
            || add_vma_to_process(process_ptr, proc.stack_start, proc.stack_end, PAGE_PRESENT as u32 | PAGE_USER as u32 | PAGE_WRITABLE as u32) != 0
        {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"create_process_vm: Failed to seed initial VMAs\n\0".as_ptr() as *const c_char,
            );
            teardown_process_mappings(process_ptr);
            free_page_frame((*page_dir_ptr).pml4_phys);
            kfree(page_dir_ptr as *mut _);
            proc.page_dir = ptr::null_mut();
            proc.process_id = INVALID_PROCESS_ID;
            return INVALID_PROCESS_ID;
        }

        let stack_map_flags = PAGE_PRESENT | PAGE_USER | PAGE_WRITABLE;
        let mut stack_pages: u32 = 0;
        if map_user_range(
            proc.page_dir,
            proc.stack_start,
            proc.stack_end,
            stack_map_flags,
            &mut stack_pages,
        ) != 0
        {
            klog_printf(
                slopos_lib::klog::KlogLevel::Info,
                b"create_process_vm: Failed to map process stack\n\0".as_ptr() as *const c_char,
            );
            teardown_process_mappings(process_ptr);
            free_page_frame((*page_dir_ptr).pml4_phys);
            kfree(page_dir_ptr as *mut _);
            proc.page_dir = ptr::null_mut();
            proc.process_id = INVALID_PROCESS_ID;
            return INVALID_PROCESS_ID;
        }
        proc.total_pages += stack_pages;

        manager.process_list = process_ptr;
        manager.num_processes += 1;
        klog_printf(
            slopos_lib::klog::KlogLevel::Info,
            b"Created process VM space for PID %u\n\0".as_ptr() as *const c_char,
            process_id,
        );
    }
    process_id
}

#[no_mangle]
pub extern "C" fn destroy_process_vm(process_id: u32) -> c_int {
    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return 0;
    }
    unsafe {
        if (*process_ptr).process_id == INVALID_PROCESS_ID {
            return 0;
        }
        klog_printf(
            slopos_lib::klog::KlogLevel::Info,
            b"Destroying process VM space for PID %u\n\0".as_ptr() as *const c_char,
            process_id,
        );
    }

    unsafe {
        teardown_process_mappings(process_ptr);
        paging_free_user_space((*process_ptr).page_dir);
        if !(*process_ptr).page_dir.is_null() {
            if (*(*process_ptr).page_dir).pml4_phys != 0 {
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
        (*process_ptr).vma_list = ptr::null_mut();
        (*process_ptr).next = ptr::null_mut();
        (*process_ptr).total_pages = 0;
        (*process_ptr).flags = 0;
        manager.num_processes = manager.num_processes.saturating_sub(1);
    }
    0
}

#[no_mangle]
pub extern "C" fn process_vm_alloc(process_id: u32, size: u64, flags: u32) -> u64 {
    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() {
        return 0;
    }
    let process = unsafe { &mut *process_ptr };
    let layout = unsafe { &*mm_get_process_layout() };

    let mut size_aligned = (size + PAGE_SIZE_4KB - 1) & !(PAGE_SIZE_4KB - 1);
    if size_aligned == 0 {
        return 0;
    }
    let start_addr = process.heap_end;
    let end_addr = start_addr + size_aligned;
    if end_addr > layout.heap_max {
        unsafe { klog_printf(slopos_lib::klog::KlogLevel::Info, b"process_vm_alloc: Heap overflow\n\0".as_ptr() as *const c_char); }
        return 0;
    }

    let mut protection_flags = flags & (PAGE_PRESENT as u32 | PAGE_WRITABLE as u32 | 0x04);
    if protection_flags == 0 {
        protection_flags = PAGE_PRESENT as u32 | PAGE_WRITABLE as u32;
    }

    let mut pages_mapped: u32 = 0;
    let mut map_flags = PAGE_PRESENT | PAGE_USER;
    if protection_flags & PAGE_WRITABLE as u32 != 0 {
        map_flags |= PAGE_WRITABLE;
    }
    if map_user_range(process.page_dir, start_addr, end_addr, map_flags, &mut pages_mapped) != 0 {
        return 0;
    }

    if add_vma_to_process(process_ptr, start_addr, end_addr, protection_flags | PAGE_USER as u32) != 0 {
        unsafe { klog_printf(slopos_lib::klog::KlogLevel::Info, b"process_vm_alloc: Failed to record VMA\n\0".as_ptr() as *const c_char); }
        unmap_user_range(process.page_dir, start_addr, end_addr);
        process.heap_end = start_addr;
        return 0;
    }

    process.heap_end = end_addr;
    process.total_pages += pages_mapped;
    start_addr
}

#[no_mangle]
pub extern "C" fn process_vm_free(process_id: u32, vaddr: u64, size: u64) -> c_int {
    let process_ptr = find_process_vm(process_id);
    if process_ptr.is_null() || size == 0 {
        return -1;
    }
    let process = unsafe { &mut *process_ptr };

    let start = vaddr & !(PAGE_SIZE_4KB - 1);
    let end = (vaddr + size + PAGE_SIZE_4KB - 1) & !(PAGE_SIZE_4KB - 1);
    if !vma_range_valid(start, end) {
        unsafe { klog_printf(slopos_lib::klog::KlogLevel::Info, b"process_vm_free: Invalid or unaligned range\n\0".as_ptr() as *const c_char); }
        return -1;
    }

    let vma = find_vma_covering(process_ptr, start, end);
    if vma.is_null() {
        unsafe { klog_printf(slopos_lib::klog::KlogLevel::Info, b"process_vm_free: Range not covered by a VMA\n\0".as_ptr() as *const c_char); }
        return -1;
    }

    let freed = unmap_and_free_range(process_ptr, start, end);

    unsafe {
        if start == (*vma).start_addr && end == (*vma).end_addr {
            remove_vma_from_process(process_ptr, (*vma).start_addr, (*vma).end_addr);
        } else if start == (*vma).start_addr {
            (*vma).start_addr = end;
        } else if end == (*vma).end_addr {
            (*vma).end_addr = start;
        } else {
            let right_start = end;
            let right_end = (*vma).end_addr;
            (*vma).end_addr = start;
            if add_vma_to_process(process_ptr, right_start, right_end, (*vma).flags) != 0 {
                klog_printf(
                    slopos_lib::klog::KlogLevel::Info,
                    b"process_vm_free: Failed to create right split VMA\n\0".as_ptr() as *const c_char,
                );
                return -1;
            }
        }
        merge_adjacent(process_ptr, vma);
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

#[no_mangle]
pub extern "C" fn init_process_vm() -> c_int {
    let mut manager = VM_MANAGER.lock();
    manager.num_processes = 0;
    manager.next_process_id = 1;
    manager.active_process = ptr::null_mut();
    manager.process_list = ptr::null_mut();
    for i in 0..MAX_PROCESSES {
        manager.processes[i] = ProcessVm::empty();
    }
    unsafe {
        klog_printf(
            slopos_lib::klog::KlogLevel::Debug,
            b"Process VM manager initialized\n\0".as_ptr() as *const c_char,
        );
    }
    0
}

#[no_mangle]
pub extern "C" fn get_process_vm_stats(total_processes: *mut u32, active_processes: *mut u32) {
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

#[no_mangle]
pub extern "C" fn get_current_process_id() -> u32 {
    let manager = VM_MANAGER.lock();
    if manager.active_process.is_null() {
        0
    } else {
        unsafe { (*manager.active_process).process_id }
    }
}

