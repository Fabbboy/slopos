use slopos_abi::addr::{PhysAddr, VirtAddr};

use crate::cow::handle_cow_fault;
use crate::demand::handle_demand_fault;
use crate::dual_paging::{ostd_get_pte_flags_4kb, ostd_map_4kb_user, ostd_mark_cow_4kb};
use crate::error::MmError;
use crate::page_alloc::{alloc_kernel_page, free_page_frame};
use crate::paging_defs::PageFlags;
use crate::process_vm::{
    create_process_vm, destroy_process_vm, init_process_vm, process_vm_clone_cow,
    process_vm_user_va_to_paddr, process_vm_with_dual_paging,
};
use slopos_abi::task::INVALID_PROCESS_ID;

/// RAII guard: owns a process VM, calls `destroy_process_vm` on drop.
///
/// The legacy `*mut ProcessPageDir` handle is no longer the
/// load-bearing path for paging — every operation flows through the
/// process's OSTD `KArc<VmSpace>`. The guard exposes thin helpers
/// (`map_test_page`, `mark_cow`, `is_cow`, `virt_to_phys`, …) that
/// drive the OSTD cursor under the per-process lock so test bodies
/// don't have to thread the lock through themselves.
pub struct ProcessVmGuard {
    pub pid: u32,
}

impl ProcessVmGuard {
    pub fn new() -> Option<Self> {
        init_process_vm();
        let pid = create_process_vm();
        if pid == INVALID_PROCESS_ID {
            return None;
        }
        Some(Self { pid })
    }

    pub fn clone_cow(&self) -> Option<Self> {
        let child_pid = process_vm_clone_cow(self.pid);
        if child_pid == INVALID_PROCESS_ID {
            return None;
        }
        Some(Self { pid: child_pid })
    }

    /// Drive `cow::handle_cow_fault` through the per-process lock.
    pub fn handle_cow_fault(&self, fault_addr: u64) -> Result<(), MmError> {
        process_vm_with_dual_paging(self.pid, |vs| handle_cow_fault(vs, fault_addr))
            .unwrap_or(Err(MmError::NullPageDir))
    }

    /// Drive `demand::handle_demand_fault` through the per-process lock.
    pub fn handle_demand_fault(&self, fault_addr: u64, error_code: u64) -> Result<(), MmError> {
        crate::process_vm::process_vm_with_dual_paging_and_region(
            self.pid,
            fault_addr,
            |vs, region| handle_demand_fault(vs, self.pid, fault_addr, error_code, &region),
        )
        .unwrap_or(Err(MmError::NullPageDir))
    }

    /// Map a 4 KiB page at `vaddr` into the test process's OSTD
    /// VmSpace. Returns the physical address that backs the new
    /// mapping, or `None` on allocation / cursor failure.
    pub fn map_test_page(&self, vaddr: u64, flags: u64) -> Option<PhysAddr> {
        let phys = alloc_kernel_page();
        if phys.is_null() {
            return None;
        }
        let result = process_vm_with_dual_paging(self.pid, |vs| {
            ostd_map_4kb_user(vs, VirtAddr::new(vaddr), phys, flags)
        });
        match result {
            Some(Ok(())) => Some(phys),
            _ => {
                free_page_frame(phys);
                None
            }
        }
    }

    /// Translate a user VA to its backing physical address (with the
    /// page-offset bits preserved, mirroring legacy
    /// `virt_to_phys_in_dir`). Returns `PhysAddr::NULL` if no leaf is
    /// present.
    pub fn virt_to_phys(&self, vaddr: u64) -> PhysAddr {
        PhysAddr::new(process_vm_user_va_to_paddr(self.pid, vaddr))
    }

    /// Mark the 4 KiB leaf at `vaddr` as copy-on-write in the OSTD
    /// VmSpace (clears `WRITABLE`, sets the COW software bit). No-op
    /// if no leaf is present at that VA.
    pub fn mark_cow(&self, vaddr: u64) {
        let _ = process_vm_with_dual_paging(self.pid, |vs| {
            let _ = ostd_mark_cow_4kb(vs, VirtAddr::new(vaddr));
        });
    }

    /// Probe whether the 4 KiB leaf at `vaddr` carries the COW marker.
    pub fn is_cow(&self, vaddr: u64) -> bool {
        process_vm_with_dual_paging(self.pid, |vs| {
            ostd_get_pte_flags_4kb(vs, VirtAddr::new(vaddr))
                .map_or(false, |f| f.contains(PageFlags::COW))
        })
        .unwrap_or(false)
    }
}

impl Drop for ProcessVmGuard {
    fn drop(&mut self) {
        destroy_process_vm(self.pid);
    }
}
