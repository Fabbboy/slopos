use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_ostd::klog_info;

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
    let is_cow = process_vm::process_vm_with_vm_space(process_id, |vs| {
        cow::is_cow_fault(error_code, vs, fault_addr)
    })
    .unwrap_or(false);

    if is_cow {
        let result = process_vm::process_vm_with_vm_space(process_id, |vs| {
            cow::handle_cow_fault(vs, fault_addr)
        });

        match result {
            Some(Ok(())) => {
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
        let result = process_vm::process_vm_with_vm_space_and_region(
            process_id,
            fault_addr,
            |vs, region| {
                demand::handle_demand_fault(vs, process_id, fault_addr, error_code, &region)
            },
        );
        if matches!(result, Some(Ok(()))) {
            return true;
        }
    }

    false
}
