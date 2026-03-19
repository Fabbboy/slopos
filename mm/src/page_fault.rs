use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_lib::{klog_debug, klog_info};

use crate::memory_layout_defs::MAX_PROCESSES;
use crate::{cow, demand, process_vm};

pub fn try_resolve_user_fault(
    fault_addr: u64,
    error_code: u64,
    process_id: u32,
    task_id: u32,
) -> bool {
    if process_id == INVALID_PROCESS_ID || (process_id as usize) >= MAX_PROCESSES {
        return false;
    }

    let page_dir = process_vm::process_vm_get_page_dir(process_id);
    if page_dir.is_null() || (page_dir as u64) < 0xffff_8000_0000_0000 {
        return false;
    }

    if cow::is_cow_fault(error_code, page_dir, fault_addr) {
        klog_debug!(
            "PF: COW fault task {} (pid {}) at cr2=0x{:x} err=0x{:x}",
            task_id,
            process_id,
            fault_addr,
            error_code
        );
        let result = cow::handle_cow_fault(page_dir, fault_addr);

        if result.is_ok() {
            klog_debug!(
                "PF: COW resolved for task {} at cr2=0x{:x}",
                task_id,
                fault_addr
            );
            return true;
        }
        klog_info!(
            "PF: COW resolution FAILED for task {} at cr2=0x{:x}",
            task_id,
            fault_addr
        );
    }

    if demand::is_demand_fault(error_code, process_id, fault_addr) {
        if demand::handle_demand_fault(page_dir, process_id, fault_addr, error_code).is_ok() {
            return true;
        }
    }

    false
}
