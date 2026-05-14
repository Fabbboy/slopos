use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_ostd::{klog_debug, klog_info};

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

    // Quick path: probe COW status WITHOUT taking the per-process lock
    // for write — the read-only `&KArc<VmSpace>` deref + cursor query
    // does the job. If the address isn't COW we fall through to the
    // demand-fault check.
    let is_cow = process_vm::process_vm_with_dual_paging(process_id, |_pd, vs| {
        cow::is_cow_fault(error_code, vs, fault_addr)
    })
    .unwrap_or(false);

    if is_cow {
        klog_debug!(
            "PF: COW fault task {} (pid {}) at cr2=0x{:x} err=0x{:x}",
            task_id,
            process_id,
            fault_addr,
            error_code
        );
        let result = process_vm::process_vm_with_dual_paging(process_id, |_pd, vs| {
            cow::handle_cow_fault(vs, fault_addr)
        });

        match result {
            Some(Ok(())) => {
                klog_debug!(
                    "PF: COW resolved for task {} at cr2=0x{:x}",
                    task_id,
                    fault_addr
                );
                return true;
            }
            Some(Err(_)) | None => {
                klog_info!(
                    "PF: COW resolution FAILED for task {} at cr2=0x{:x}",
                    task_id,
                    fault_addr
                );
            }
        }
    }

    if demand::is_demand_fault(error_code, process_id, fault_addr) {
        let result = process_vm::process_vm_with_dual_paging_and_region(
            process_id,
            fault_addr,
            |_pd, vs, region| {
                demand::handle_demand_fault(vs, process_id, fault_addr, error_code, &region)
            },
        );
        if matches!(result, Some(Ok(()))) {
            // Page-count bookkeeping must happen OUTSIDE the
            // per-process lock that `process_vm_with_dual_paging_*`
            // holds (the increment helper takes the same lock).
            process_vm::process_vm_increment_pages(process_id, 1);
            return true;
        }
    }

    false
}
