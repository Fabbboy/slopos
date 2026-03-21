use slopos_abi::addr::{PhysAddr, VirtAddr};

use crate::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame};
use crate::paging::{ProcessPageDir, map_page_4kb_in_dir};
use crate::process_vm::{
    create_process_vm, destroy_process_vm, init_process_vm, process_vm_clone_cow,
    process_vm_get_page_dir,
};
use slopos_abi::task::INVALID_PROCESS_ID;

/// RAII guard: owns a process VM + page directory, calls `destroy_process_vm` on drop.
pub struct ProcessVmGuard {
    pub pid: u32,
    pub page_dir: *mut ProcessPageDir,
}

impl ProcessVmGuard {
    pub fn new() -> Option<Self> {
        init_process_vm();
        let pid = create_process_vm();
        if pid == INVALID_PROCESS_ID {
            return None;
        }
        let page_dir = process_vm_get_page_dir(pid);
        if page_dir.is_null() {
            destroy_process_vm(pid);
            return None;
        }
        Some(Self { pid, page_dir })
    }

    pub fn clone_cow(&self) -> Option<Self> {
        let child_pid = process_vm_clone_cow(self.pid);
        if child_pid == INVALID_PROCESS_ID {
            return None;
        }
        let page_dir = process_vm_get_page_dir(child_pid);
        if page_dir.is_null() {
            destroy_process_vm(child_pid);
            return None;
        }
        Some(Self {
            pid: child_pid,
            page_dir,
        })
    }
}

impl Drop for ProcessVmGuard {
    fn drop(&mut self) {
        destroy_process_vm(self.pid);
    }
}

/// Allocate a zeroed page frame and map it at `vaddr`. Frees the frame on mapping failure.
pub fn map_test_page(page_dir: *mut ProcessPageDir, vaddr: u64, flags: u64) -> Option<PhysAddr> {
    let phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if phys.is_null() {
        return None;
    }
    if map_page_4kb_in_dir(page_dir, VirtAddr::new(vaddr), phys, flags) != 0 {
        free_page_frame(phys);
        return None;
    }
    Some(phys)
}
