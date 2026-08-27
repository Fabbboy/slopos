use slopos_abi::addr::{PhysAddr, VirtAddr};

use crate::cow::handle_cow_fault;
use crate::demand::handle_demand_fault;
use crate::error::MmError;

use crate::paging_defs::PageFlags;
use crate::process_vm::{
    create_process_vm, destroy_process_vm, init_process_vm, process_vm_clone_cow,
    process_vm_user_va_to_paddr, process_vm_with_vm_space,
};
use crate::user_mappings::{ostd_get_pte_flags_4kb, ostd_map_4kb_user_fresh, ostd_mark_cow_4kb};
use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_ostd::process::ProcessId;

/// Owns a process VM; the helpers drive the OSTD cursor under the per-process
/// lock so test bodies do not thread that lock themselves.
pub struct ProcessVmGuard {
    pub process: ProcessId,
}

impl ProcessVmGuard {
    pub fn new() -> Option<Self> {
        init_process_vm();
        let pid = create_process_vm();
        if pid == INVALID_PROCESS_ID {
            return None;
        }
        Some(Self {
            process: ProcessId::resolve(pid)?,
        })
    }

    pub fn pid(&self) -> u32 {
        self.process.id()
    }

    pub fn clone_cow(&self) -> Option<Self> {
        let child_pid = process_vm_clone_cow(self.process);
        if child_pid == INVALID_PROCESS_ID {
            return None;
        }
        Some(Self {
            process: ProcessId::resolve(child_pid)?,
        })
    }

    pub fn handle_cow_fault(&self, fault_addr: u64) -> Result<(), MmError> {
        process_vm_with_vm_space(self.process, |vs| handle_cow_fault(vs, fault_addr))
            .unwrap_or(Err(MmError::NoAddressSpace))
    }

    pub fn handle_demand_fault(&self, fault_addr: u64, error_code: u64) -> Result<(), MmError> {
        crate::process_vm::process_vm_with_vm_space_and_region(
            self.process,
            fault_addr,
            |vs, region| handle_demand_fault(vs, fault_addr, error_code, &region),
        )
        .unwrap_or(Err(MmError::NoAddressSpace))
    }

    /// Returns the physical address backing the new mapping.
    pub fn map_test_page(&self, vaddr: u64, flags: u64) -> Option<PhysAddr> {
        process_vm_with_vm_space(self.process, |vs| {
            ostd_map_4kb_user_fresh(vs, VirtAddr::new(vaddr), flags).ok()
        })
        .flatten()
    }

    /// Page-offset bits are preserved; `PhysAddr::NULL` if no leaf is present.
    pub fn virt_to_phys(&self, vaddr: u64) -> PhysAddr {
        PhysAddr::new(process_vm_user_va_to_paddr(self.process, vaddr))
    }

    /// Clears `WRITABLE` and sets the COW software bit; no-op if no leaf is
    /// present at `vaddr`.
    pub fn mark_cow(&self, vaddr: u64) {
        let _ = process_vm_with_vm_space(self.process, |vs| {
            let _ = ostd_mark_cow_4kb(vs, VirtAddr::new(vaddr));
        });
    }

    pub fn is_cow(&self, vaddr: u64) -> bool {
        process_vm_with_vm_space(self.process, |vs| {
            ostd_get_pte_flags_4kb(vs, VirtAddr::new(vaddr))
                .map_or(false, |f| f.contains(PageFlags::COW))
        })
        .unwrap_or(false)
    }
}

impl Drop for ProcessVmGuard {
    fn drop(&mut self) {
        destroy_process_vm(self.process);
    }
}
